// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/// Profile-Guided Optimization (PGO) data collected during interpreted execution.
///
/// During the interpreter warmup phase each method accumulates:
/// - **Branch counts** — taken vs. not-taken at each conditional-branch bytecode.
/// - **Receiver type counts** — receiver class frequency at each invokevirtual /
///   invokeinterface site.
///
/// These profiles are consumed when the JIT compiles the method:
/// - Branch counts guide code-layout decisions (prefer the hot direction as fall-through).
/// - Receiver type counts pre-populate Monomorphic Inline Cache (MIC) slots so the
///   common-case virtual dispatch is a direct call from the very first JIT execution.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// Global profiling enable gate (perf-critical: AUDIT CRIT-3/CRIT-5/HIGH-7)
// ---------------------------------------------------------------------------
//
// The interpreter calls `ProfileStore::record_branch` / `record_backedge`
// from ~13 sites on every conditional branch / back-edge. Even with FxHashMap
// the per-call overhead used to include a parking_lot::Mutex acquire and an
// `Arc<str>` clone for the MethodKey — together visible in the
// `interpreter_counting_loop` benchmark.
//
// The fix is twofold:
//   1. Gate the profile-recording calls behind a single AtomicBool
//      (`PROFILING_ENABLED`).  Default is `false` so warmup-free workloads
//      (microbenchmarks, AOT'd JDK code) pay only one relaxed atomic load
//      per branch.  Profilers / tiered managers flip it to `true` when
//      they want to drive JIT promotion.
//   2. Replace the global `Mutex<HashMap>` with a `RwLock<HashMap<Arc<...>>>`
//      so concurrent recorders hit the read-lock path and only the rare
//      first-time-insert path takes the write-lock.

static PROFILING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable or disable profile recording globally.
///
/// When disabled (the default), `ProfileStore::record_branch`,
/// `record_backedge`, `record_receiver`, and `record_trip_complete`
/// all return immediately after a single relaxed atomic load.  This keeps
/// the interpreter hot loop free of HashMap / lock overhead until a
/// profiler is actively driving JIT compilation.
#[inline]
pub fn enable_profiling(b: bool) {
    PROFILING_ENABLED.store(b, Ordering::Relaxed);
}

/// Returns whether profile recording is currently enabled.
#[inline(always)]
pub fn is_profiling_enabled() -> bool {
    PROFILING_ENABLED.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Key type
// ---------------------------------------------------------------------------

/// Identifies a single method for profile-store lookup.
///
/// Methods from different classes can share the same `method_name`+`descriptor`,
/// so the `class_id` is included to disambiguate them.
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct MethodKey {
    pub class_id: u32,
    pub method_name: Arc<str>,
    pub descriptor: Arc<str>,
}

// ---------------------------------------------------------------------------
// Per-branch profile
// ---------------------------------------------------------------------------

/// Taken / not-taken counts for a single conditional-branch instruction.
#[derive(Default, Clone, Debug)]
pub struct BranchCounts {
    /// Number of times the branch target was taken (condition TRUE).
    pub taken: u32,
    /// Number of times the fall-through path was executed (condition FALSE).
    pub not_taken: u32,
}

impl BranchCounts {
    /// Returns `true` when the branch is overwhelmingly not-taken
    /// (taken < 10 % of total observations with at least 20 samples).
    pub fn is_usually_not_taken(&self) -> bool {
        let total = self.taken.saturating_add(self.not_taken);
        total >= 20 && self.taken * 10 < total
    }

    /// Returns `true` when the branch is overwhelmingly taken
    /// (taken > 90 % of total observations with at least 20 samples).
    pub fn is_usually_taken(&self) -> bool {
        let total = self.taken.saturating_add(self.not_taken);
        total >= 20 && self.taken * 10 > total * 9
    }
}

// ---------------------------------------------------------------------------
// Per-call-site receiver type profile
// ---------------------------------------------------------------------------

/// Receiver class frequency at a single invokevirtual / invokeinterface site.
/// Maps raw class_id to observation count.
/// T10.9.B: FxHashMap — class_id-keyed receiver count, hot path during
/// interpreter warmup.
pub type ReceiverCounts = FxHashMap<u32, u32>;

/// Returns the dominant receiver class (most frequent) and its count if it
/// exceeds `min_fraction_pct` percent of total observations.
pub fn dominant_receiver(counts: &ReceiverCounts, min_fraction_pct: u32) -> Option<u32> {
    let total: u32 = counts.values().copied().sum();
    if total == 0 {
        return None;
    }
    let (&dom_class, &dom_count) = counts.iter().max_by_key(|(_, &c)| c)?;
    if dom_count * 100 >= total * min_fraction_pct {
        Some(dom_class)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Per-loop trip count profile
// ---------------------------------------------------------------------------

/// Trip count profile for a loop, keyed by its back-edge bytecode PC.
#[derive(Default, Clone, Debug)]
pub struct LoopTripProfile {
    /// Total number of back-edges observed (sum of all trip counts).
    pub backedge_count: u64,
    /// Number of times the loop was entered (each entry eventually produces
    /// one trip-complete event with its iteration count).
    pub entry_count: u32,
    /// Sum of per-entry trip counts (for computing average).
    pub total_trips: u64,
}

impl LoopTripProfile {
    /// Record a single back-edge execution.
    #[inline]
    pub fn record_backedge(&mut self) {
        self.backedge_count = self.backedge_count.saturating_add(1);
    }

    /// Record the trip count for one loop entry (called when the loop exits).
    #[inline]
    pub fn record_trip_complete(&mut self, trip: u32) {
        self.entry_count = self.entry_count.saturating_add(1);
        self.total_trips = self.total_trips.saturating_add(trip as u64);
    }

    /// Average trip count across all observed entries.
    pub fn avg_trip_count(&self) -> f64 {
        if self.entry_count == 0 {
            0.0
        } else {
            self.total_trips as f64 / self.entry_count as f64
        }
    }

    /// Suggest an unroll factor based on the observed trip count.
    ///
    /// Returns `None` if the loop is cold (<100 back-edges) or the average
    /// trip count is too large (>128, where unrolling gives diminishing returns).
    pub fn suggests_unroll_factor(&self, max_factor: usize) -> Option<usize> {
        if self.backedge_count < 100 {
            return None; // not hot enough
        }
        let avg = self.avg_trip_count();
        if avg > 128.0 || avg < 2.0 {
            return None; // too large or trivial
        }
        // Choose factor: avg≤8 → 4x, avg≤32 → 2x, else None
        let factor = if avg <= 8.0 {
            4usize
        } else if avg <= 32.0 {
            2
        } else {
            return None;
        };
        Some(factor.min(max_factor))
    }
}

// ---------------------------------------------------------------------------
// Per-method profile
// ---------------------------------------------------------------------------

/// Collected profile data for a single method.
/// T10.9.B: FxHashMap — bytecode-PC keys, hot path per-branch during warmup.
#[derive(Default)]
pub struct MethodProfile {
    /// Branch counts keyed by bytecode PC.
    pub branches: FxHashMap<usize, BranchCounts>,
    /// Receiver type counts keyed by bytecode PC of the invoke instruction.
    pub receivers: FxHashMap<usize, ReceiverCounts>,
    /// Loop trip profiles keyed by back-edge bytecode PC.
    pub loops: FxHashMap<usize, LoopTripProfile>,
}

impl MethodProfile {
    /// Record a branch observation at `pc`.
    ///
    /// `taken` is `true` when the branch was taken (condition was true).
    #[inline]
    pub fn record_branch(&mut self, pc: usize, taken: bool) {
        let entry = self.branches.entry(pc).or_default();
        if taken {
            entry.taken = entry.taken.saturating_add(1);
        } else {
            entry.not_taken = entry.not_taken.saturating_add(1);
        }
    }

    /// Record a receiver type observation at invoke `pc`.
    #[inline]
    pub fn record_receiver(&mut self, pc: usize, class_id: u32) {
        let entry = self.receivers.entry(pc).or_default();
        *entry.entry(class_id).or_insert(0) += 1;
    }

    /// Record a back-edge execution at the given PC.
    #[inline]
    pub fn record_backedge(&mut self, pc: usize) {
        self.loops.entry(pc).or_default().record_backedge();
    }

    /// Record a completed trip count for a loop at the given back-edge PC.
    #[inline]
    pub fn record_trip_complete(&mut self, backedge_pc: usize, trip: u32) {
        self.loops
            .entry(backedge_pc)
            .or_default()
            .record_trip_complete(trip);
    }
}

// ---------------------------------------------------------------------------
// Global profile store
// ---------------------------------------------------------------------------

/// Number of shards for the per-method maps. Must be a power of two so
/// the `hash & (PROFILE_SHARDS-1)` mapping is a single mask.
///
/// Round-11 cross-cutting HIGH-1: sharded the previously-monolithic
/// `RwLock<FxHashMap<...>>` into 16 buckets keyed by the method's
/// `MethodKey` hash (slow path) or the SipHash fingerprint of
/// `(class_id, method_name, descriptor)` (borrowed-key fast path).
/// Concurrent recorders that hash to different shards proceed without
/// contention; the rwlock contention previously visible in
/// `record_branch_borrowed`/`get_profile` under multi-thread JIT
/// warmup is divided by 16.
///
/// Each shard preserves the same `methods` + `name_index` pair as the
/// pre-shard design, so the round-7 CRIT-3 two-phase clone-then-lock
/// pattern (for `snapshot_all` / `get_profile`) still applies — it now
/// runs per-shard.
const PROFILE_SHARDS: usize = 16;

/// One shard of the per-method profile store. Each shard owns its own
/// `methods` rwlock + `name_index` rwlock, identical in structure to
/// the pre-sharding monolithic store.
///
/// Round-11 cross-cutting HIGH-1: extracted from `ProfileStore` so we
/// can hold an array of these and dispatch by hash.
struct ProfileShard {
    methods: parking_lot::RwLock<FxHashMap<MethodKey, Arc<parking_lot::Mutex<MethodProfile>>>>,
    name_index:
        parking_lot::RwLock<FxHashMap<u64, (MethodKey, Arc<parking_lot::Mutex<MethodProfile>>)>>,
}

impl ProfileShard {
    fn new() -> Self {
        Self {
            methods: parking_lot::RwLock::new(FxHashMap::default()),
            name_index: parking_lot::RwLock::new(FxHashMap::default()),
        }
    }
}

/// Compute the shard index for an owned `MethodKey`.
#[inline]
fn shard_for_key(key: &MethodKey) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    (h.finish() as usize) & (PROFILE_SHARDS - 1)
}

/// Compute the shard index and 64-bit fingerprint for a borrowed
/// `(class_id, method_name, descriptor)` triple in a single hash pass.
/// The fingerprint is used as the `name_index` key, and its low bits
/// pick the shard — so a fingerprint hit on shard `s` is guaranteed to
/// belong to the same shard as a `MethodKey`-hash lookup for the same
/// triple in the slow path (verified below).
#[inline]
fn shard_and_fingerprint_for_borrowed(
    class_id: u32,
    method_name: &str,
    descriptor: &str,
) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    class_id.hash(&mut h);
    method_name.hash(&mut h);
    descriptor.hash(&mut h);
    let fingerprint = h.finish();
    // Shard from fingerprint low bits. The slow path computes shard
    // via `shard_for_key` (MethodKey::hash) which uses the SAME hasher
    // and the SAME field order, so both paths land on the same shard
    // for the same triple.
    let shard = (fingerprint as usize) & (PROFILE_SHARDS - 1);
    (shard, fingerprint)
}

/// Global repository of all method profiles collected during interpreted execution.
/// T10.9.B: FxHashMap — MethodKey (ClassId+name+desc, internal) and packed u64
/// keys, hot path on every interpreter invoke.
///
/// **Lock strategy (AUDIT CRIT-3/CRIT-5/HIGH-7 fix, round-11 sharded):**
/// - Sharded across `PROFILE_SHARDS` independent rwlocks keyed by the
///   `MethodKey` hash. Contention is divided by the shard count; threads
///   recording into different methods land on different shards in the
///   common case.
/// - Per shard: outer `RwLock` so concurrent recorders share a read-lock
///   for the common "method already exists" path. Only the rare first-time
///   insert escalates to a write-lock.
/// - Each `MethodProfile` is wrapped in `Arc<parking_lot::Mutex<_>>` so the
///   inner `FxHashMap`s can be mutated per-method without serialising every
///   recorder on a single global Mutex.
/// - All recording paths short-circuit on the global `PROFILING_ENABLED`
///   atomic — interpreter hot-loops pay one relaxed load per branch when
///   profiling is off (the default).
pub struct ProfileStore {
    /// Per-shard `(methods, name_index)` pair. Picked by hashing the
    /// `MethodKey` (slow path) or the borrowed triple's fingerprint
    /// (fast path).
    shards: [ProfileShard; PROFILE_SHARDS],
    /// Per-method invocation counters for JIT warmup gating.
    /// Keyed by `(class_id << 32 | method_hash)` packed into a `u64` for fast lookup.
    invocation_counts: parking_lot::Mutex<FxHashMap<u64, u32>>,
    /// PERF (round-5 vm #7): auxiliary index keyed by a 64-bit fingerprint
    /// of `(class_id, method_name, descriptor)`. See `ProfileShard::name_index`
    /// for the per-shard storage — this struct field is intentionally absent
    /// post-sharding; each shard carries its own index.
    ///
    /// round-7 fix (bug 2): the cached value is `(MethodKey,
    /// Arc<Mutex<MethodProfile>>)`. The cached `MethodKey` is compared
    /// against the requested `(class_id, &name, &descriptor)` after every
    /// fingerprint hit; on the (extremely rare) SipHash-fingerprint
    /// collision we fall through to the canonical `methods` slow path
    /// instead of silently returning the wrong method's profile slot.
    ///
    /// round-7 fix (bug 2): diagnostic counter — number of fingerprint
    /// hits that turned out to be a false positive (different
    /// `MethodKey` than the requested one) and fell through to the slow
    /// path.  Expected to be 0 in normal operation; non-zero indicates
    /// a SipHash collision (or, more likely, a logic bug).
    name_index_collisions: std::sync::atomic::AtomicU64,

    /// round-9 fix (HIGH): diagnostic counter — number of times the
    /// re-probe in the collision-counter path observed an entry that
    /// matched the queried key, indicating a legal concurrent insert
    /// raced with us between our first probe and the re-probe. This is
    /// a BENIGN race (the map is insert-only; another writer simply
    /// published the same key we were about to look up), not a logic
    /// bug, so we soft-count it instead of panicking via a
    /// `debug_assert!` as the previous round-8 code did. Non-zero
    /// values here are expected under concurrent load and do not
    /// indicate corruption.
    name_index_benign_races: std::sync::atomic::AtomicU64,
}

impl ProfileStore {
    pub fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| ProfileShard::new()),
            invocation_counts: parking_lot::Mutex::new(FxHashMap::default()),
            name_index_collisions: std::sync::atomic::AtomicU64::new(0),
            name_index_benign_races: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Reclaim every profile and warmup counter owned by an unloaded class.
    pub fn invalidate_class(&self, class_id: u32) {
        self.invocation_counts
            .lock()
            .retain(|packed, _| (*packed >> 32) as u32 != class_id);
        for shard in &self.shards {
            let mut methods = shard.methods.write();
            methods.retain(|key, _| key.class_id != class_id);
            let mut index = shard.name_index.write();
            index.retain(|_, (key, _)| key.class_id != class_id);
        }
    }

    /// round-9 fix (HIGH): observed benign re-probe races (between the
    /// first probe and the collision re-probe, another writer published
    /// an entry that matches the queried key). Exposed for diagnostics
    /// only; non-zero values are normal under concurrent load.
    pub fn name_index_benign_races(&self) -> u64 {
        self.name_index_benign_races
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// round-7 fix (bug 2): observed name-index fingerprint collisions
    /// (cached slot's `MethodKey` did not match the queried triple, so
    /// we fell through to the canonical `methods` lookup).  Exposed
    /// for diagnostics; non-zero values usually indicate either a
    /// SipHash collision (probability ~2.7e-10 per pair at 100k
    /// methods) or a logic bug.
    pub fn name_index_collisions(&self) -> u64 {
        self.name_index_collisions
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Increment the invocation counter for a method identified by the packed key
    /// `(class_id << 32) | method_hash`. Returns the new count.
    ///
    /// Called from the interpreter's cached bytecode dispatch to gate JIT compilation
    /// behind a warmup threshold instead of compiling on the second invocation.
    #[inline]
    pub fn increment_invocation(&self, packed_key: u64) -> u32 {
        let mut counts = self.invocation_counts.lock();
        let entry = counts.entry(packed_key).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    /// Fetch (or insert) the per-method profile slot.  Returns a cheap `Arc`
    /// to the inner Mutex so callers can release the outer lock immediately.
    ///
    /// Round-11 cross-cutting HIGH-1: dispatches to one of `PROFILE_SHARDS`
    /// rwlocks based on the `MethodKey` hash. Concurrent recorders for
    /// different methods rarely contend.
    #[inline]
    fn get_or_insert(&self, key: &MethodKey) -> Arc<parking_lot::Mutex<MethodProfile>> {
        let shard = &self.shards[shard_for_key(key)];
        // Fast path: read-lock + lookup.  Most calls hit this branch.
        {
            let read = shard.methods.read();
            if let Some(slot) = read.get(key) {
                return Arc::clone(slot);
            }
        }
        // Slow path: upgrade to write-lock and double-check (another writer
        // may have inserted between us dropping the read-lock and acquiring
        // the write-lock).
        let mut write = shard.methods.write();
        if let Some(slot) = write.get(key) {
            return Arc::clone(slot);
        }
        let slot = Arc::new(parking_lot::Mutex::new(MethodProfile::default()));
        write.insert(key.clone(), Arc::clone(&slot));
        slot
    }

    /// Borrowed-key variant of [`get_or_insert`]: avoids the two
    /// `Arc::clone` atomic refcount bumps that the owned-key path pays on
    /// every profiled hit (round-5 vm #7).
    ///
    /// Strategy: the hot path is a profile *hit* (the (class, method, desc)
    /// triple has already been seen).  On a hit we never need to construct
    /// a `MethodKey` at all — we walk the map looking for the unique entry
    /// whose three fields equal the borrowed triple.  Since the underlying
    /// hash-map is keyed by `MethodKey` and we cannot probe by `(u32, &str,
    /// &str)` on stable Rust (raw-entry API is nightly), we instead keep an
    /// auxiliary `name_index` index that the cold insert path populates;
    /// hot lookups consult that index by `class_id` (its key type is a
    /// cheap `u64`) and then verify the names match by `&str` comparison
    /// — no `Arc::clone` on hit.
    ///
    /// The owned `MethodKey` is constructed exactly once per (class,
    /// method, descriptor) triple, inside the cold insert branch.
    #[inline]
    fn get_or_insert_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
    ) -> Arc<parking_lot::Mutex<MethodProfile>> {
        // Compute shard + 64-bit fingerprint in a single hash pass.
        // Round-11 cross-cutting HIGH-1: fingerprint low bits also pick
        // the shard, ensuring borrowed-key fast-path lookups land on
        // the same shard the slow `MethodKey`-keyed path would (both
        // use the same `DefaultHasher` over the same `(class_id,
        // method_name, descriptor)` tuple in the same order).
        let (shard_idx, fingerprint) =
            shard_and_fingerprint_for_borrowed(class_id, method_name, descriptor);
        let shard = &self.shards[shard_idx];

        // Lock-order discipline (round-7 CRIT-2): the canonical order
        // is `methods` > `name_index` (see `docs/lock-order.md`). All
        // code paths in this function take `methods` *before*
        // `name_index`, never the other way around.
        //
        // Fast path: probe `name_index` briefly, drop it, then take
        // `methods.read()` to verify and clone the slot.  No nested
        // acquisition — the two reads are sequential.
        //
        // round-7 fix (bug 2): the cached value is now `(MethodKey,
        // Arc<...>)`.  After a fingerprint hit we verify the cached
        // key matches the query triple; on mismatch (collision) we
        // bump the diagnostic counter and fall through to the slow
        // path, which uses the canonical `MethodKey`-equality map.
        //
        // The verification happens *under* the read-lock so we only
        // pay one `Arc::clone` on the slot (matching the pre-fix
        // hot-path cost) and no `MethodKey` clone.  Verification
        // itself is u32 compare + two `&str` equality short-circuits.
        let cached: Option<Arc<parking_lot::Mutex<MethodProfile>>> = {
            let idx = shard.name_index.read();
            match idx.get(&fingerprint) {
                Some((cached_key, slot))
                    if cached_key.class_id == class_id
                        && cached_key.method_name.as_ref() == method_name.as_ref()
                        && cached_key.descriptor.as_ref() == descriptor.as_ref() =>
                {
                    Some(Arc::clone(slot))
                }
                Some(_) => None, // collision — handled below
                None => None,
            }
        };
        if let Some(slot) = cached {
            return slot;
        }
        // Distinguish "fingerprint missed entirely" from "fingerprint
        // hit but key mismatched (collision)": re-probe briefly to bump
        // the counter only on real collisions.  The probability of
        // either is so low this re-probe never shows up in a profile.
        //
        // LOAD-BEARING: this re-probe assumes the index is INSERT-ONLY
        // (no eviction).  Between the first read above and this
        // second read, the only legal transition is "absent -> present"
        // (handled by the slow path below).  If a future change adds
        // eviction or fingerprint-slot replacement, an entry that
        // mismatched on the first probe could become an unrelated entry
        // by the second probe — same fingerprint, different real key —
        // and we'd under-count collisions.  If eviction is introduced,
        // swap this counter for an atomic increment INSIDE the first
        // read's `Some(_)` collision arm above.
        {
            let idx = shard.name_index.read();
            if let Some((cached_key, _)) = idx.get(&fingerprint) {
                // Round-9 fix (HIGH): the previous `debug_assert!` here
                // panicked on a perfectly legal benign race — between
                // our first probe and this re-probe, another writer
                // can legitimately publish an entry for the SAME key
                // we were looking up (insert-only map, slow path wins
                // the race). The assertion was guarding against
                // eviction (which the map does not perform), but the
                // matching-entry-after-mismatch case it tripped on IS
                // the benign race the comment itself describes.
                //
                // Soft-count the two cases separately so the diagnostic
                // information is preserved without crashing under load.
                if cached_key.class_id == class_id
                    && cached_key.method_name.as_ref() == method_name.as_ref()
                    && cached_key.descriptor.as_ref() == descriptor.as_ref()
                {
                    // Benign race: another writer inserted our key
                    // between the two probes. Not a collision; do not
                    // bump the collision counter.
                    self.name_index_benign_races
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else {
                    // Real fingerprint collision: different MethodKey
                    // shares our SipHash fingerprint.
                    self.name_index_collisions
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        // Slow path: take `shard.methods.write()` FIRST, then
        // `shard.name_index.write()` nested inside it.  Build the
        // owned `MethodKey` (one-time per triple) for canonical
        // insertion into `methods`.
        let slot = Arc::new(parking_lot::Mutex::new(MethodProfile::default()));
        let key = MethodKey {
            class_id,
            method_name: Arc::clone(method_name),
            descriptor: Arc::clone(descriptor),
        };
        let mut write = shard.methods.write();
        // Race: another writer may have inserted between us dropping the
        // index read-lock and acquiring `methods.write()`.  Re-probe
        // under the exclusive lock and return the existing slot if so.
        if let Some(existing) = write.get(&key) {
            let existing = Arc::clone(existing);
            // methods (held) > name_index (canonical order).
            //
            // round-7 fix (bug 2): only overwrite the name_index entry
            // if its current fingerprint slot is empty or already
            // points at this same key — we must not stomp a different
            // method's slot just because our fingerprint collides
            // with theirs.  When there's already a conflicting entry
            // we leave it in place; the next borrowed-lookup for the
            // *other* method will hit, verify, and return correctly,
            // while lookups for *this* method will collide-then-fall-
            // through to here, which is correct (if slow).
            let mut idx = shard.name_index.write();
            let should_insert = match idx.get(&fingerprint) {
                None => true,
                Some((existing_key, _)) => existing_key == &key,
            };
            if should_insert {
                idx.insert(fingerprint, (key, Arc::clone(&existing)));
            }
            return existing;
        }
        write.insert(key.clone(), Arc::clone(&slot));
        // Still under methods.write() — nested name_index.write() in
        // canonical order.
        //
        // round-7 fix (bug 2): same conflict check as above before
        // inserting our key into the auxiliary index.
        let mut idx = shard.name_index.write();
        let should_insert = match idx.get(&fingerprint) {
            None => true,
            Some((existing_key, _)) => existing_key == &key,
        };
        if should_insert {
            idx.insert(fingerprint, (key, Arc::clone(&slot)));
        }
        drop(idx);
        drop(write);
        slot
    }

    /// Borrowed-key counterpart of [`record_branch`] — see
    /// [`get_or_insert_borrowed`] for the rationale.
    #[inline]
    pub fn record_branch_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        pc: usize,
        taken: bool,
    ) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        slot.lock().record_branch(pc, taken);
    }

    /// Borrowed-key counterpart of [`record_backedge`].
    #[inline]
    pub fn record_backedge_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        backedge_pc: usize,
    ) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        slot.lock().record_backedge(backedge_pc);
    }

    /// Borrowed-key counterpart of [`record_receiver`].
    #[inline]
    pub fn record_receiver_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        pc: usize,
        receiver_class_id: u32,
    ) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        slot.lock().record_receiver(pc, receiver_class_id);
    }

    /// Record a branch observation.  Called from the interpreter hot-loop.
    ///
    /// Returns immediately when profiling is disabled (the global default),
    /// keeping the interpreter dispatch loop free of HashMap/lock traffic.
    #[inline]
    pub fn record_branch(&self, key: &MethodKey, pc: usize, taken: bool) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert(key);
        slot.lock().record_branch(pc, taken);
    }

    /// Record a receiver type observation.  Called from the interpreter hot-loop.
    #[inline]
    pub fn record_receiver(&self, key: &MethodKey, pc: usize, class_id: u32) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert(key);
        slot.lock().record_receiver(pc, class_id);
    }

    /// Record a loop back-edge execution.  Called from the interpreter hot-loop.
    #[inline]
    pub fn record_backedge(&self, key: &MethodKey, backedge_pc: usize) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert(key);
        slot.lock().record_backedge(backedge_pc);
    }

    /// Record a completed loop trip count.  Called when a loop exits.
    #[inline]
    pub fn record_trip_complete(&self, key: &MethodKey, backedge_pc: usize, trip: u32) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert(key);
        slot.lock().record_trip_complete(backedge_pc, trip);
    }

    /// Retrieve a snapshot of the profile for the given method, if any.
    ///
    /// Round-7 CRIT-3 sibling fix: same two-phase pattern as
    /// `snapshot_all` — clone the `Arc<Mutex<...>>` under the outer
    /// read-lock, drop the outer guard, then lock the per-slot Mutex
    /// independently.  Prevents the per-slot lock from being acquired
    /// while `methods.read()` is held.
    pub fn get_profile(&self, key: &MethodKey) -> Option<MethodProfile> {
        // Round-11 cross-cutting HIGH-1: dispatch to the owning shard.
        let shard = &self.shards[shard_for_key(key)];
        let slot_arc = {
            let read = shard.methods.read();
            Arc::clone(read.get(key)?)
        };
        let p = slot_arc.lock();
        // Snapshot: clone branch + receiver + loop maps
        Some(MethodProfile {
            branches: p.branches.clone(),
            receivers: p.receivers.clone(),
            loops: p.loops.clone(),
        })
    }

    /// Snapshot all method profiles: returns (MethodKey, MethodProfile) pairs.
    /// Used by AOT training to bulk-sync JIT profile data to the AOT recorder.
    ///
    /// Round-7 CRIT-3 fix: two-phase snapshot.  Phase 1 clones the (key, Arc)
    /// pairs under the outer read-lock and drops it.  Phase 2 locks each
    /// per-slot Mutex with the outer guard already released — so no thread
    /// holding a slot lock can deadlock against a writer trying to take
    /// `methods.write()`.
    pub fn snapshot_all(&self) -> Vec<(MethodKey, MethodProfile)> {
        // Round-11 cross-cutting HIGH-1: walk every shard. The
        // two-phase clone-then-lock pattern still applies per shard so
        // no per-slot Mutex is held while the shard's outer rwlock is
        // also held.
        let mut arcs: Vec<(MethodKey, Arc<parking_lot::Mutex<MethodProfile>>)> = Vec::new();
        for shard in self.shards.iter() {
            // Phase 1: snapshot the (key, Arc<Mutex<...>>) pairs.
            // Holding only the shard's outer read-lock — no per-slot
            // lock taken yet.
            let read = shard.methods.read();
            arcs.extend(read.iter().map(|(k, slot)| (k.clone(), Arc::clone(slot))));
            // Outer shard lock drops at end of scope; next iteration
            // moves to a different shard's lock entirely.
        }
        // Phase 2: outer locks released; iterate locking each slot
        // independently.  Slot lock is now leaf-level.
        arcs.into_iter()
            .map(|(k, slot)| {
                let p = slot.lock();
                (
                    k,
                    MethodProfile {
                        branches: p.branches.clone(),
                        receivers: p.receivers.clone(),
                        loops: p.loops.clone(),
                    },
                )
            })
            .collect()
    }

    /// Snapshot all invocation counts: returns (packed_key, count) pairs.
    pub fn snapshot_invocation_counts(&self) -> Vec<(u64, u32)> {
        let counts = self.invocation_counts.lock();
        counts.iter().map(|(&k, &v)| (k, v)).collect()
    }
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests record into the global ProfileStore — gate must be on or
    /// `record_*` is a no-op.  Each test calls this guard before driving
    /// the store; the inner `Mutex` serialises gate transitions so a
    /// parallel test that depends on the disabled-default doesn't observe
    /// `true` mid-run.
    fn with_profiling_enabled<R>(f: impl FnOnce() -> R) -> R {
        static GATE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        let _g = GATE.lock();
        let prev = is_profiling_enabled();
        enable_profiling(true);
        let r = f();
        enable_profiling(prev);
        r
    }

    fn make_key(class_id: u32) -> MethodKey {
        MethodKey {
            class_id,
            method_name: Arc::from("test"),
            descriptor: Arc::from("()V"),
        }
    }

    #[test]
    fn test_branch_counts_thresholds() {
        // Not enough samples — neither flag set.
        let sparse = BranchCounts {
            taken: 5,
            not_taken: 5,
        };
        assert!(!sparse.is_usually_taken());
        assert!(!sparse.is_usually_not_taken());

        // Usually taken (>90 %).
        let hot = BranchCounts {
            taken: 95,
            not_taken: 5,
        };
        assert!(hot.is_usually_taken());
        assert!(!hot.is_usually_not_taken());

        // Usually not taken (<10 %).
        let cold = BranchCounts {
            taken: 1,
            not_taken: 99,
        };
        assert!(!cold.is_usually_taken());
        assert!(cold.is_usually_not_taken());

        // Borderline — exactly 90 % (not strictly > 90 %).
        let borderline = BranchCounts {
            taken: 18,
            not_taken: 2,
        };
        assert!(!borderline.is_usually_taken(), "90% is not >90%");
    }

    #[test]
    fn test_dominant_receiver_above_threshold() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 85);
        counts.insert(2, 15);
        // 85% ≥ 80% threshold → dominant = class 1
        assert_eq!(dominant_receiver(&counts, 80), Some(1));
    }

    #[test]
    fn test_dominant_receiver_below_threshold() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 70);
        counts.insert(2, 30);
        // 70% < 80% threshold → no dominant receiver
        assert_eq!(dominant_receiver(&counts, 80), None);
    }

    #[test]
    fn test_profile_store_branch_accumulates() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(42);
            for _ in 0..15 {
                store.record_branch(&key, 10, true);
            }
            for _ in 0..5 {
                store.record_branch(&key, 10, false);
            }
            let profile = store.get_profile(&key).unwrap();
            let counts = &profile.branches[&10];
            assert_eq!(counts.taken, 15);
            assert_eq!(counts.not_taken, 5);
            assert!(!counts.is_usually_taken(), "<20 samples");

            // Add enough to make it >90% taken.
            for _ in 0..80 {
                store.record_branch(&key, 10, true);
            }
            let profile = store.get_profile(&key).unwrap();
            let counts = &profile.branches[&10];
            // 95 taken / 5 not-taken = 95% > 90% with ≥20 samples
            assert_eq!(counts.taken, 95);
            assert!(counts.is_usually_taken());
        });
    }

    #[test]
    fn test_profile_store_receiver_accumulates() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(7);
            for _ in 0..90 {
                store.record_receiver(&key, 5, 100);
            }
            for _ in 0..10 {
                store.record_receiver(&key, 5, 200);
            }
            let profile = store.get_profile(&key).unwrap();
            let rcounts = &profile.receivers[&5];
            assert_eq!(dominant_receiver(rcounts, 80), Some(100));
        });
    }

    /// Disabled-by-default sanity: with PROFILING_ENABLED=false the recorder
    /// must remain a no-op (no MethodKey clone, no HashMap insert).
    ///
    /// Uses the same gate as `with_profiling_enabled` to avoid racing
    /// against tests that flip the flag on.
    #[test]
    fn profiling_disabled_is_noop() {
        with_profiling_enabled(|| {
            // Within the guard, switch the gate back off for this scope so
            // we can assert the no-op behaviour without a parallel test
            // flipping it on between calls.
            enable_profiling(false);
            let store = ProfileStore::new();
            let key = make_key(123);
            for _ in 0..100 {
                store.record_branch(&key, 1, true);
                store.record_backedge(&key, 1);
                store.record_receiver(&key, 1, 0);
                store.record_trip_complete(&key, 1, 4);
            }
            assert!(
                store.get_profile(&key).is_none(),
                "no profile entry should be created while profiling is disabled"
            );
        });
    }

    // ===== M29 snapshot_all / snapshot_invocation_counts =====

    #[test]
    fn m29_snapshot_all_returns_all_methods() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let k1 = make_key(1);
            let k2 = MethodKey {
                class_id: 2,
                method_name: Arc::from("other"),
                descriptor: Arc::from("(I)V"),
            };
            store.record_branch(&k1, 5, true);
            store.record_branch(&k2, 10, false);
            let all = store.snapshot_all();
            assert_eq!(all.len(), 2);
        });
    }

    #[test]
    fn m29_snapshot_all_empty_store() {
        let store = ProfileStore::new();
        let all = store.snapshot_all();
        assert!(all.is_empty());
    }

    #[test]
    fn m29_snapshot_invocation_counts() {
        let store = ProfileStore::new();
        store.increment_invocation(0x0001_0000_ABCD);
        store.increment_invocation(0x0001_0000_ABCD);
        store.increment_invocation(0x0002_0000_1234);
        let counts = store.snapshot_invocation_counts();
        assert_eq!(counts.len(), 2);
        let abcd = counts.iter().find(|(k, _)| *k == 0x0001_0000_ABCD);
        assert_eq!(abcd.unwrap().1, 2);
    }

    #[test]
    fn m29_snapshot_all_preserves_branch_data() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(99);
            for _ in 0..10 {
                store.record_branch(&key, 42, true);
            }
            for _ in 0..5 {
                store.record_branch(&key, 42, false);
            }
            let all = store.snapshot_all();
            assert_eq!(all.len(), 1);
            let (_, profile) = &all[0];
            let counts = &profile.branches[&42];
            assert_eq!(counts.taken, 10);
            assert_eq!(counts.not_taken, 5);
        });
    }

    #[test]
    fn m29_snapshot_all_preserves_receiver_data() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(88);
            store.record_receiver(&key, 20, 500);
            store.record_receiver(&key, 20, 500);
            store.record_receiver(&key, 20, 600);
            let all = store.snapshot_all();
            assert_eq!(all.len(), 1);
            let (_, profile) = &all[0];
            let rcounts = &profile.receivers[&20];
            assert_eq!(rcounts[&500], 2);
            assert_eq!(rcounts[&600], 1);
        });
    }

    // ===== S38 loop trip profile tests =====

    #[test]
    fn s38_loop_trip_profile_basic() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..50 {
            ltp.record_backedge();
        }
        assert_eq!(ltp.backedge_count, 50);
        assert_eq!(ltp.entry_count, 0);
        assert_eq!(ltp.avg_trip_count(), 0.0);
    }

    #[test]
    fn s38_loop_trip_suggests_unroll() {
        let mut ltp = LoopTripProfile::default();
        // Simulate a loop with avg trip count 8: 100 entries × 8 trips = 800 back-edges
        for _ in 0..800 {
            ltp.record_backedge();
        }
        for _ in 0..100 {
            ltp.record_trip_complete(8);
        }
        assert!((ltp.avg_trip_count() - 8.0).abs() < 0.01);
        assert_eq!(ltp.suggests_unroll_factor(8), Some(4)); // avg≤8 → 4x
    }

    #[test]
    fn s38_loop_trip_no_unroll_cold() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..50 {
            ltp.record_backedge();
        }
        // < 100 back-edges → not hot enough
        assert_eq!(ltp.suggests_unroll_factor(8), None);
    }

    #[test]
    fn s38_loop_trip_no_unroll_large_trip() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..200 {
            ltp.record_backedge();
        }
        for _ in 0..2 {
            ltp.record_trip_complete(100); // avg = 100
        }
        // avg > 128? No, avg=100. But the factor logic: avg≤8→4, avg≤32→2, else None
        // avg=100 > 32 → None
        assert_eq!(ltp.suggests_unroll_factor(8), None);
    }

    #[test]
    fn s38_loop_trip_medium_suggests_2x() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..500 {
            ltp.record_backedge();
        }
        for _ in 0..25 {
            ltp.record_trip_complete(20); // avg = 20
        }
        assert_eq!(ltp.suggests_unroll_factor(8), Some(2)); // avg≤32 → 2x
    }

    #[test]
    fn s38_profile_store_backedge_accumulates() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(55);
            for _ in 0..200 {
                store.record_backedge(&key, 42);
            }
            for _ in 0..10 {
                store.record_trip_complete(&key, 42, 20);
            }
            let profile = store.get_profile(&key).unwrap();
            let ltp = &profile.loops[&42];
            assert_eq!(ltp.backedge_count, 200);
            assert_eq!(ltp.entry_count, 10);
            assert!((ltp.avg_trip_count() - 20.0).abs() < 0.01);
        });
    }

    #[test]
    fn s38_snapshot_all_preserves_loop_data() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(66);
            for _ in 0..100 {
                store.record_backedge(&key, 10);
            }
            store.record_trip_complete(&key, 10, 50);
            let all = store.snapshot_all();
            assert_eq!(all.len(), 1);
            let (_, profile) = &all[0];
            let ltp = &profile.loops[&10];
            assert_eq!(ltp.backedge_count, 100);
            assert_eq!(ltp.entry_count, 1);
        });
    }

    /// T10.9.B — smoke test: ProfileStore uses FxHashMap for methods/invocation_counts.
    /// Verifies insert/lookup semantics are preserved after the HashMap → FxHashMap swap.
    #[test]
    fn t10_9_b_fx_hashmap_swap_smoke() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            // Populate a few hundred methods and invocation counts.
            for i in 0..300u32 {
                let key = make_key(i);
                store.record_branch(&key, (i as usize) * 4, i % 2 == 0);
                store.increment_invocation((i as u64) << 32);
            }
            // Snapshot should show all 300 methods.
            let snap = store.snapshot_all();
            assert_eq!(snap.len(), 300, "every inserted method should be present");
            let ivc = store.snapshot_invocation_counts();
            assert_eq!(ivc.len(), 300, "every invocation slot should be present");
            // Spot-check a specific key survives the hash migration.
            let k = make_key(42);
            let profile = store.get_profile(&k).unwrap();
            assert!(
                profile.branches.contains_key(&(42usize * 4)),
                "FxHashMap lookup should find the inserted PC"
            );
        });
    }
}

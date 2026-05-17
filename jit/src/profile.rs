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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
pub fn dominant_receiver(
    counts: &ReceiverCounts,
    min_fraction_pct: u32,
) -> Option<u32> {
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
        self.loops.entry(backedge_pc).or_default().record_trip_complete(trip);
    }
}

// ---------------------------------------------------------------------------
// Global profile store
// ---------------------------------------------------------------------------

/// Global repository of all method profiles collected during interpreted execution.
/// T10.9.B: FxHashMap — MethodKey (ClassId+name+desc, internal) and packed u64
/// keys, hot path on every interpreter invoke.
///
/// **Lock strategy (AUDIT CRIT-3/CRIT-5/HIGH-7 fix):**
/// - Outer `RwLock` so concurrent recorders share a read-lock for the common
///   "method already exists" path.  Only the rare first-time insert escalates
///   to a write-lock.
/// - Each `MethodProfile` is wrapped in `Arc<parking_lot::Mutex<_>>` so the
///   inner `FxHashMap`s can be mutated per-method without serialising every
///   recorder on a single global Mutex.
/// - All recording paths short-circuit on the global `PROFILING_ENABLED`
///   atomic — interpreter hot-loops pay one relaxed load per branch when
///   profiling is off (the default).
pub struct ProfileStore {
    methods: parking_lot::RwLock<FxHashMap<MethodKey, Arc<parking_lot::Mutex<MethodProfile>>>>,
    /// Per-method invocation counters for JIT warmup gating.
    /// Keyed by `(class_id << 32 | method_hash)` packed into a `u64` for fast lookup.
    invocation_counts: parking_lot::Mutex<FxHashMap<u64, u32>>,
}

impl ProfileStore {
    pub fn new() -> Self {
        Self {
            methods: parking_lot::RwLock::new(FxHashMap::default()),
            invocation_counts: parking_lot::Mutex::new(FxHashMap::default()),
        }
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
    #[inline]
    fn get_or_insert(&self, key: &MethodKey) -> Arc<parking_lot::Mutex<MethodProfile>> {
        // Fast path: read-lock + lookup.  Most calls hit this branch.
        {
            let read = self.methods.read();
            if let Some(slot) = read.get(key) {
                return Arc::clone(slot);
            }
        }
        // Slow path: upgrade to write-lock and double-check (another writer
        // may have inserted between us dropping the read-lock and acquiring
        // the write-lock).
        let mut write = self.methods.write();
        if let Some(slot) = write.get(key) {
            return Arc::clone(slot);
        }
        let slot = Arc::new(parking_lot::Mutex::new(MethodProfile::default()));
        write.insert(key.clone(), Arc::clone(&slot));
        slot
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
    pub fn get_profile(&self, key: &MethodKey) -> Option<MethodProfile> {
        let read = self.methods.read();
        let slot = read.get(key)?;
        let p = slot.lock();
        // Snapshot: clone branch + receiver + loop maps
        Some(MethodProfile {
            branches: p.branches.clone(),
            receivers: p.receivers.clone(),
            loops: p.loops.clone(),
        })
    }

    /// Snapshot all method profiles: returns (MethodKey, MethodProfile) pairs.
    /// Used by AOT training to bulk-sync JIT profile data to the AOT recorder.
    pub fn snapshot_all(&self) -> Vec<(MethodKey, MethodProfile)> {
        let read = self.methods.read();
        read.iter()
            .map(|(k, slot)| {
                let p = slot.lock();
                (
                    k.clone(),
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
        let sparse = BranchCounts { taken: 5, not_taken: 5 };
        assert!(!sparse.is_usually_taken());
        assert!(!sparse.is_usually_not_taken());

        // Usually taken (>90 %).
        let hot = BranchCounts { taken: 95, not_taken: 5 };
        assert!(hot.is_usually_taken());
        assert!(!hot.is_usually_not_taken());

        // Usually not taken (<10 %).
        let cold = BranchCounts { taken: 1, not_taken: 99 };
        assert!(!cold.is_usually_taken());
        assert!(cold.is_usually_not_taken());

        // Borderline — exactly 90 % (not strictly > 90 %).
        let borderline = BranchCounts { taken: 18, not_taken: 2 };
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
            assert!(store.get_profile(&key).is_none(),
                "no profile entry should be created while profiling is disabled");
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
            for _ in 0..10 { store.record_branch(&key, 42, true); }
            for _ in 0..5 { store.record_branch(&key, 42, false); }
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

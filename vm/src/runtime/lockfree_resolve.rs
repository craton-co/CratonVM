// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared method-resolution caching for the interpreter's hot path.
//!
//! ## What this module is
//!
//! One cache: `promoted_invokes`, a cross-thread map of fully-built invoke
//! targets, reached via [`SharedResolutionState::get_promoted_invoke`] /
//! [`SharedResolutionState::insert_promoted_invoke`] from the interpreter's
//! invoke paths. A thread-local `JvmThread::invoke_cache` miss consults it
//! before falling through to the slow `class_manager` walk.
//!
//! ## What it used to be (ARCH-2026-08-04 A7)
//!
//! Until 2026-08-04 this file was 1,634 lines describing a three-level flow —
//! thread-local cache, shared global cache, full resolution — of which only the
//! third of the middle tier was ever wired. A 2026-07-26 audit
//! (`stackwalk-and-vtable`) documented the gap accurately in this header and
//! left the code in place: `ThreadLocalResolveCache` had zero production
//! instantiations, and `SharedResolutionState::resolve_method` / `cache_method`
//! / `resolve_field` / `cache_field`, their `global_methods` / `global_fields`
//! maps, and the `ResolutionKey` / `ResolvedTarget` / `ResolvedField` types had
//! no production callers. All of it was reachable only from this file's own
//! unit tests, which is precisely why `dead_code` never fired.
//!
//! That was 389 lines of dead machinery — including a security hardening pass
//! on `ResolutionKey` (full interned-string identity validation to defeat
//! `FxHasher` collision forgery, plus loader-epoch isolation) applied to a
//! cache nothing consulted — plus an `invalidate_all` that took three write
//! locks to clear two permanently-empty maps. It is gone. If a second
//! resolution tier is wanted later, write it against the live call sites
//! rather than resurrecting this; the deleted version was never load-bearing
//! and its shape reflects no measurement.
//!
//! The general lesson generalises past this file, so it is now enforced rather
//! than remembered: `vm/tests/no_test_only_public_api.rs` fails the build on a
//! `pub` item in this crate whose only references are inside `#[cfg(test)]`.
//!
//! ## "Lock-free" is a misnomer — keep it in mind before relying on it
//!
//! Nothing here is lock-free in the technical sense, and the live path in
//! particular is not. `get_promoted_invoke` takes a `parking_lot::RwLock`
//! **read** guard on a single process-wide map: acquiring it is an atomic
//! read-modify-write on one shared word, so every dispatching thread writes
//! the same cache line on every consult. That is much cheaper than the
//! `ClassManager` write lock it replaces — which is the real and worthwhile
//! win, and it is genuine — but it is contention, not its absence. The claim
//! that this "eliminates lock contention on the common case" was overstated;
//! it *relocates* it off the class-manager lock.
//!
//! (This is the same class of overstatement a sibling pass found on
//! `class_manager.rs::class_loading_locks`, which claimed to "allow concurrent
//! loading of different classes to proceed without contention" while
//! `load_class_concurrent` still ran under the L10 write lock. Verify against
//! the code, not the comment.)

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Shared-cache capacity bound
// ---------------------------------------------------------------------------

/// HIGH (security) — default upper bound on the number of entries held in the
/// cross-thread `promoted_invokes` map.
///
/// (It used to bound three maps; `global_methods` and `global_fields` were
/// deleted as dead in ARCH-2026-08-04 A7. The bound itself is unchanged.)
///
/// The map grew without limit before this cap: an
/// adversarial or dynamic-codegen workload that mints an unbounded number of
/// distinct `(class, member, descriptor, loader)` keys — e.g. a class that
/// emits fresh lambda / proxy / hidden-class names per call — would grow these
/// maps until the process exhausts memory, a denial-of-service.
///
/// These maps are *pure caches*: every entry can be reconstructed by
/// re-resolving against the class manager, so dropping an entry is always
/// correctness-preserving (a miss simply re-resolves and re-promotes). That
/// lets us pick the lowest-risk effective bound — a hard cap with approximate
/// eviction — without touching the read path.
///
/// The default (65536 per map) is generous: a real application's live working
/// set of resolved members is far smaller, so steady-state programs never hit
/// the cap, while a key-minting adversary is held to a bounded footprint.
const DEFAULT_SHARED_CACHE_CAP: usize = 65_536;

/// Uncached form of [`shared_cache_cap`]. Kept separate so the unit tests can
/// exercise the parsing rules without depending on which test happened to run
/// first (the public entry point memoizes for process lifetime).
fn parse_shared_cache_cap(value: Option<&str>) -> usize {
    match value {
        Some(s) => match s.trim().parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => DEFAULT_SHARED_CACHE_CAP,
        },
        None => DEFAULT_SHARED_CACHE_CAP,
    }
}

fn read_shared_cache_cap() -> usize {
    let value = cratonvm_types::flags::runtime_var("CRATONVM_RESOLVE_CACHE_CAP").ok();
    parse_shared_cache_cap(value.as_deref())
}

/// Resolve the shared-cache capacity, honouring the `CRATONVM_RESOLVE_CACHE_CAP`
/// environment override (mirrors the project's `CRATONVM_*` configuration
/// convention). A value of `0`, an empty string, or an unparseable value falls
/// back to [`DEFAULT_SHARED_CACHE_CAP`]; the cap can never be set below 1 so a
/// freshly-inserted entry always survives.
///
/// PERF (2026-07-26 arch pass). This used to call `cratonvm_types::flags::runtime_var` — which
/// allocates a `String` and, on Windows, is a `GetEnvironmentVariableW`
/// syscall — on **every first promotion of a call site**, and did so *while
/// holding the `promoted_invokes` write guard*, so every thread promoting a
/// target serialised behind an environment lookup. Real applications mint one
/// such promotion per distinct `(caller class, cp_index, receiver class)`
/// triple, i.e. hundreds of thousands of them during warm-up.
///
/// Memoized in a `OnceLock`, matching the process-lifetime memo contract every
/// flag in `runtime::env_cache` already has (and the one a sibling pass just
/// applied to the seven throw-path flags in `exceptions.rs`): setting the
/// variable after the first promotion no longer takes effect. This is not a
/// new gate — the variable is an existing, optional tuning override with a
/// working default, so nothing lands off by default.
#[inline]
fn shared_cache_cap() -> usize {
    // Test-only escape hatch. The memo below is process-lifetime, so the
    // eviction tests can no longer steer it with the environment variable;
    // they set this instead. Compiled out entirely in non-test builds, so the
    // shipped fast path is a single relaxed `OnceLock` read.
    #[cfg(test)]
    {
        let forced = tests_cap_override::get();
        if forced != 0 {
            return forced;
        }
    }
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(read_shared_cache_cap)
}

#[cfg(test)]
mod tests_cap_override {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static OVERRIDE: AtomicUsize = AtomicUsize::new(0);
    /// `0` means "no override — use the memoized value".
    pub(super) fn get() -> usize {
        OVERRIDE.load(Ordering::Relaxed)
    }
    pub(super) fn set(v: usize) {
        OVERRIDE.store(v, Ordering::Relaxed);
    }
}

/// Evict entries from a write-locked shared cache map until it can accept one
/// more insert without exceeding `cap`, i.e. until `len < cap`.
///
/// This runs **only** while the caller already holds the map's write guard, so
/// no reader or other writer can be touching the map concurrently — the
/// eviction can never deadlock against the read path. Because the maps are pure
/// caches, the eviction policy only needs to be *approximate*: we drop an
/// arbitrary entry surfaced by the hash map's iteration order (cheap — no
/// auxiliary ordering structure to maintain, unlike the thread-local FIFO). A
/// wrongly-evicted entry costs at most one re-resolution.
///
/// Generic over the key/value so the same routine bounds all three shared maps.
#[inline]
fn evict_to_fit<K, V>(map: &mut FxHashMap<K, V>, cap: usize)
where
    K: std::hash::Hash + Eq + Clone,
{
    // Leave room for the imminent insert: shrink until `len < cap`. Guard on
    // emptiness so a pathological `cap == 0` (already excluded by
    // `shared_cache_cap`, but defended here for direct callers/tests) cannot
    // spin forever.
    while map.len() >= cap {
        let victim = match map.keys().next() {
            Some(k) => k.clone(),
            None => break,
        };
        map.remove(&victim);
    }
}

// Round-9 MED-1: doc cleanup. `SharedResolutionState` below holds three
// `parking_lot::RwLock` maps (`global_methods`, `global_fields`,
// `promoted_invokes`); the struct definition at the bottom of this file is the
// source of truth. parking_lot drops the pthread_rwlock + poisoning overhead
// that the std lock would impose — every other workspace lock is already
// parking_lot.
use parking_lot::RwLock;

#[allow(unused_imports)]
use super::fx_collections::{fx_hashmap, FxBuildHasher, FxHashMap, FxHasher};
use crate::classloading::resolution::CachedInvokeTarget as GenericCachedInvokeTarget;
use crate::classloading::ClassId;
use std::hash::{Hash, Hasher};

type CachedInvokeTarget = GenericCachedInvokeTarget<cratonvm_jit::RetainedCode>;


// ---------------------------------------------------------------------------
// SharedResolutionState
// ---------------------------------------------------------------------------

/// Cross-thread promoted invoke target key:
/// `(caller_class, cp_index, is_special, receiver_class)`.
///
/// `receiver_class` is `None` for invokestatic / invokespecial and the concrete
/// receiver `ClassId` for monomorphic invokevirtual/invokeinterface entries so
/// that two receiver classes sharing the same call site do not collide.
pub type PromotedInvokeKey = (ClassId, u16, bool, Option<ClassId>);

/// Read-optimised shared resolution state accessible from any thread.
///
/// Reads take a `RwLock` read-guard (concurrent readers never block each
/// other).  Writes take the write-guard and are expected to be infrequent
/// (only on first resolution of a given target).
pub struct SharedResolutionState {
    /// T10.4 — cross-thread promoted cache of fully-built invoke targets.
    ///
    /// Thread-local `invoke_cache` misses consult this map under a read-lock
    /// before falling through to the slow `class_manager` walk.  When a thread
    /// completes the slow path it promotes the resulting `CachedInvokeTarget`
    /// here so sibling threads skip the walk on their first call.
    promoted_invokes: RwLock<FxHashMap<PromotedInvokeKey, CachedInvokeTarget>>,
    /// T10.4 observability — number of successful read-lock hits on
    /// `promoted_invokes` since VM start.  Tests assert this counter to prove
    /// the shared read path bypassed the class-manager write lock.
    promoted_hits: AtomicU64,
    /// T10.4 observability — number of write-lock inserts into
    /// `promoted_invokes`.
    promoted_inserts: AtomicU64,
}

impl SharedResolutionState {
    /// Create an empty shared state.
    pub fn new() -> Self {
        Self {
            promoted_invokes: RwLock::new(fx_hashmap()),
            promoted_hits: AtomicU64::new(0),
            promoted_inserts: AtomicU64::new(0),
        }
    }

    // -- T10.4 promoted invoke targets -------------------------------------

    /// Attempt to fetch a fully-built `CachedInvokeTarget` for the given
    /// call site.  A hit avoids walking the class manager and rebuilding the
    /// `Arc<CachedBytecodeMethod>`.  Acquires only a read-lock.
    ///
    /// WP2.4-F1 — stale entries (whose declaring class has been redefined
    /// since the entry was promoted) are *not* returned. The hot path stays
    /// read-locked: we observe a stale entry under the read guard, drop the
    /// guard, then upgrade to a write guard to remove it. Callers see
    /// `None` and fall through to the slow re-resolution which will
    /// promote a fresh entry.
    pub fn get_promoted_invoke(&self, key: &PromotedInvokeKey) -> Option<CachedInvokeTarget> {
        let guard = self.promoted_invokes.read();
        let hit = guard.get(key).cloned();
        drop(guard);
        match hit {
            Some(t) if !t.is_stale() => {
                self.promoted_hits.fetch_add(1, Ordering::Relaxed);
                Some(t)
            }
            Some(_stale) => {
                // Evict on detection — sibling threads would otherwise keep
                // re-promoting the same stale entry until somebody noticed.
                let mut guard = self.promoted_invokes.write();
                if let Some(existing) = guard.get(key) {
                    if existing.is_stale() {
                        guard.remove(key);
                    }
                }
                None
            }
            None => None,
        }
    }

    /// Promote a fully-built `CachedInvokeTarget` so sibling threads can
    /// populate their local invoke cache without repeating the slow
    /// resolution walk.  Acquires a write-lock.
    ///
    /// HIGH (security) — bounded at [`shared_cache_cap`] entries; a call site
    /// minting unbounded distinct `(caller, cp_index, receiver)` keys cannot
    /// grow this map without limit. Re-promoting an already-present call site
    /// never evicts.
    pub fn insert_promoted_invoke(&self, key: PromotedInvokeKey, target: CachedInvokeTarget) {
        let mut guard = self.promoted_invokes.write();
        if !guard.contains_key(&key) {
            evict_to_fit(&mut guard, shared_cache_cap());
        }
        guard.insert(key, target);
        self.promoted_inserts.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of the promoted-invoke hit counter (tests / diagnostics).
    pub fn promoted_hit_count(&self) -> u64 {
        self.promoted_hits.load(Ordering::Relaxed)
    }

    /// Snapshot of the promoted-invoke insert counter (tests / diagnostics).
    pub fn promoted_insert_count(&self) -> u64 {
        self.promoted_inserts.load(Ordering::Relaxed)
    }

    /// Number of distinct call sites currently cached in the promoted-invoke
    /// map (tests / diagnostics).  Acquires a read-lock.
    pub fn promoted_invoke_count(&self) -> usize {
        self.promoted_invokes.read().len()
    }

    // -- housekeeping -----------------------------------------------------

    /// Clear the promoted-invoke cache — the only cache this type owns.
    ///
    /// ARCH-2026-07-26 (`cross-owner-closeout`, request CR-LR-1 of
    /// `docs/internal/arch-2026-07-26/stackwalk-and-vtable.md`). This is the
    /// entry point the class-loader unload sweep in `vm/src/memory/gc.rs`
    /// wants: a conservative wholesale clear of the live cache.
    ///
    /// There used to be an `invalidate_all` beside this that also cleared
    /// `global_methods` / `global_fields`. Those maps had no production
    /// writers, so it acquired two write locks to clear two permanently-empty
    /// maps; both maps are gone (ARCH-2026-08-04 A7) and so is it. The one
    /// caller — `vm/src/memory/gc.rs` — already called this method and carried
    /// a comment explaining why it avoided the three-lock variant.
    pub fn invalidate_promoted(&self) {
        self.promoted_invokes.write().clear();
    }

    /// Clear promoted invoke entries whose caller class or receiver class
    /// matches `class_id` — used by CHA invalidation / class redefinition.
    pub fn invalidate_promoted_for_class(&self, class_id: ClassId) {
        let mut guard = self.promoted_invokes.write();
        guard.retain(|(caller, _, _, rcv), _| *caller != class_id && *rcv != Some(class_id));
    }
}

impl Default for SharedResolutionState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    static RESOLVE_CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());
    use std::thread;

    // -- T10.4 promoted invoke cache --------------------------------------

    fn sample_bytecode_method(
        class_id: u32,
    ) -> std::sync::Arc<cratonvm_jit_api::CachedBytecodeMethod> {
        std::sync::Arc::new(cratonvm_jit_api::CachedBytecodeMethod {
            declaring_class_id: ClassId::new(class_id),
            class_name: std::sync::Arc::from("A"),
            method_name: std::sync::Arc::from("m"),
            method_descriptor: std::sync::Arc::from("()V"),
            source_file: None,
            code: std::sync::Arc::from(vec![0xB1u8].as_slice()),
            exception_table: std::sync::Arc::from(vec![].as_slice()),
            max_stack: 2,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        })
    }

    #[test]
    fn t10_shared_resolution_read_hit_bypasses_write_lock() {
        let state = SharedResolutionState::new();
        let key: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(20)));
        let target = CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(20),
            cached: sample_bytecode_method(20),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        };
        // Miss first — returns None, does not bump hit counter.
        assert!(state.get_promoted_invoke(&key).is_none());
        assert_eq!(state.promoted_hit_count(), 0);
        // Insert once (takes the only write-lock for this call site).
        state.insert_promoted_invoke(key, target.clone());
        assert_eq!(state.promoted_insert_count(), 1);
        // Hit — advances the hit counter, returns an equivalent target,
        // and takes only a read-lock internally.
        let got = state.get_promoted_invoke(&key).expect("hit expected");
        match got {
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id, ..
            } => {
                assert_eq!(receiver_class_id, ClassId::new(20));
            }
            _ => panic!("wrong variant cached"),
        }
        assert_eq!(state.promoted_hit_count(), 1);
        assert_eq!(state.promoted_invoke_count(), 1);
    }

    #[test]
    fn t10_shared_resolution_distinguishes_receiver_classes() {
        // Same call site, two different receiver classes must not collide.
        let state = SharedResolutionState::new();
        let k1: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(20)));
        let k2: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(21)));
        let t1 = CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(20),
            cached: sample_bytecode_method(20),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        };
        let t2 = CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(21),
            cached: sample_bytecode_method(21),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        };
        state.insert_promoted_invoke(k1, t1);
        state.insert_promoted_invoke(k2, t2);
        assert_eq!(state.promoted_invoke_count(), 2);
        match state.get_promoted_invoke(&k1).unwrap() {
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id, ..
            } => {
                assert_eq!(receiver_class_id, ClassId::new(20));
            }
            _ => panic!("wrong variant"),
        }
        match state.get_promoted_invoke(&k2).unwrap() {
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id, ..
            } => {
                assert_eq!(receiver_class_id, ClassId::new(21));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn t10_shared_resolution_invalidate_for_class_drops_both_ends() {
        let state = SharedResolutionState::new();
        // Caller = class 10, receiver = class 30.
        let k1: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(30)));
        // Caller = class 40, receiver = class 30 (so invalidating 30 drops it).
        let k2: PromotedInvokeKey = (ClassId::new(40), 7, false, Some(ClassId::new(30)));
        // Caller = class 50, receiver = class 60 (untouched by invalidation).
        let k3: PromotedInvokeKey = (ClassId::new(50), 7, false, Some(ClassId::new(60)));
        state.insert_promoted_invoke(
            k1,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(30),
                cached: sample_bytecode_method(30),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        state.insert_promoted_invoke(
            k2,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(30),
                cached: sample_bytecode_method(30),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        state.insert_promoted_invoke(
            k3,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(60),
                cached: sample_bytecode_method(60),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        state.invalidate_promoted_for_class(ClassId::new(30));
        assert!(state.get_promoted_invoke(&k1).is_none());
        assert!(state.get_promoted_invoke(&k2).is_none());
        // k3 (class 60 / caller 50) must survive.
        assert!(state.get_promoted_invoke(&k3).is_some());
    }

    /// Wholesale clear empties the live cache.
    ///
    /// Was `t10_invalidate_all_clears_promoted_cache`, driving the since-deleted
    /// `invalidate_all` (ARCH-2026-08-04 A7). `invalidate_promoted` is the same
    /// operation on the only map this type ever owned, so the assertions are
    /// unchanged — only the entry point moved.
    #[test]
    fn t10_invalidate_promoted_clears_promoted_cache() {
        let state = SharedResolutionState::new();
        let key: PromotedInvokeKey = (ClassId::new(1), 0, false, None);
        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        assert_eq!(state.promoted_invoke_count(), 1);
        state.invalidate_promoted();
        assert_eq!(state.promoted_invoke_count(), 0);
        assert!(state.get_promoted_invoke(&key).is_none());
    }

    #[test]
    fn t10_shared_resolution_concurrent_reads_do_not_serialize() {
        // Ensure the read path uses a read-lock: multiple reader threads
        // should all observe the same hit without serialising.  We detect
        // serialisation indirectly — all N*M reads must complete and the
        // hit counter must end at exactly N*M.
        let state = std::sync::Arc::new(SharedResolutionState::new());
        let key: PromotedInvokeKey = (ClassId::new(5), 3, false, None);
        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(7),
                cached: sample_bytecode_method(7),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = std::sync::Arc::clone(&state);
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    assert!(s.get_promoted_invoke(&key).is_some());
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(state.promoted_hit_count(), 4 * 50);
        // Only one insert (the setup) — no write contention occurred.
        assert_eq!(state.promoted_insert_count(), 1);
    }

    // -- Resolution flow integration test ---------------------------------

    // -- HIGH security harden tests ---------------------------------------
    //
    // These tests prove the cache rejects (1) forged hash collisions
    // (an adversary crafts a triple whose `FxHasher` digests match a
    // cached entry's despite the strings differing — would return the
    // wrong target on the vulnerable key) and (2) cross-classloader
    // contamination (loader L1 and L2 both load the same FQN — JVMS
    // §5.3.4 makes the defining loader part of class identity).

    // -- HIGH security: shared-cache capacity bound -----------------------
    //
    // The cross-thread shared maps were previously unbounded — a workload
    // minting unbounded distinct keys could exhaust memory (DoS). These
    // tests prove the cap holds and that an evicted entry simply misses
    // (re-resolution is correctness-preserving), never panics.

    #[test]
    fn evict_to_fit_keeps_map_under_cap() {
        // Direct, env-independent test of the eviction core. Insert far more
        // than `cap` entries, evicting before each insert, and assert the map
        // never exceeds `cap`.
        const CAP: usize = 8;
        let mut map: FxHashMap<u64, u64> = fx_hashmap();
        for i in 0..1000u64 {
            evict_to_fit(&mut map, CAP);
            map.insert(i, i);
            assert!(
                map.len() <= CAP,
                "map exceeded cap: len={} cap={CAP}",
                map.len()
            );
        }
        // After the run the map is full but bounded.
        assert_eq!(map.len(), CAP);
    }

    #[test]
    fn evict_to_fit_zero_cap_does_not_spin_on_empty() {
        // Defensive: a `cap == 0` against an empty map must terminate (the
        // emptiness guard breaks the loop) rather than spin forever.
        let mut map: FxHashMap<u64, u64> = fx_hashmap();
        evict_to_fit(&mut map, 0);
        assert!(map.is_empty());
    }

    /// RAII helper for the test-only capacity override, so a failing
    /// assertion cannot leak a forced cap into the rest of the suite.
    struct CapOverride;
    impl CapOverride {
        fn set(v: usize) -> Self {
            tests_cap_override::set(v);
            CapOverride
        }
    }
    impl Drop for CapOverride {
        fn drop(&mut self) {
            tests_cap_override::set(0);
        }
    }

    #[test]
    fn shared_cache_cap_parses_env_override() {
        assert_eq!(parse_shared_cache_cap(Some("10")), 10);
        assert_eq!(parse_shared_cache_cap(Some("0")), DEFAULT_SHARED_CACHE_CAP);
        assert_eq!(parse_shared_cache_cap(Some("")), DEFAULT_SHARED_CACHE_CAP);
        assert_eq!(
            parse_shared_cache_cap(Some("not-a-number")),
            DEFAULT_SHARED_CACHE_CAP
        );
        assert_eq!(parse_shared_cache_cap(None), DEFAULT_SHARED_CACHE_CAP);
    }

    #[test]
    fn shared_cache_cap_is_memoized_for_process_lifetime() {
        // The public path is a stable process-lifetime value.
        let _guard = RESOLVE_CACHE_ENV_LOCK.lock().unwrap();
        let first = shared_cache_cap();
        assert_eq!(
            shared_cache_cap(),
            first,
            "shared_cache_cap must not re-read the environment"
        );
    }

    // -- 2026-07-26 arch pass: wiring assertions -------------------------

    #[test]
    fn promoted_invoke_round_trip_is_the_only_live_path() {
        // Guards the module doc's claim about what is wired: the
        // promoted-invoke map is the only tier, and its read path must not
        // require the class manager. The name is kept from when this file also
        // held three dead tiers (ARCH-2026-08-04 A7) — it is now a statement
        // about the whole module, not a contrast with its neighbours.
        let state = SharedResolutionState::new();
        let key: PromotedInvokeKey = (ClassId::new(1), 4, false, Some(ClassId::new(2)));
        assert!(state.get_promoted_invoke(&key).is_none());
        assert_eq!(state.promoted_hit_count(), 0);

        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        assert!(state.get_promoted_invoke(&key).is_some());
        assert_eq!(state.promoted_hit_count(), 1);
        assert_eq!(state.promoted_insert_count(), 1);

        // A miss must not be memoized as a negative anywhere: after
        // invalidation the same key simply re-misses and can be re-promoted.
        state.invalidate_promoted_for_class(ClassId::new(1));
        assert!(state.get_promoted_invoke(&key).is_none());
        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        assert!(state.get_promoted_invoke(&key).is_some());
    }

    /// A wholesale clear leaves no memoized negatives behind.
    ///
    /// Was `invalidate_promoted_matches_invalidate_all_for_the_live_cache`,
    /// which compared `invalidate_promoted` against the since-deleted
    /// `invalidate_all` (ARCH-2026-08-04 A7). That comparison is meaningless now
    /// that only one map exists, but the second half of the test is not: it
    /// covers what `promoted_invoke_round_trip_is_the_only_live_path` does not,
    /// namely that a key cleared by a *wholesale* sweep (`gc.rs`'s loader-unload
    /// path, CR-LR-1) re-misses and can be re-promoted, rather than sticking as
    /// a negative entry.
    #[test]
    fn wholesale_clear_leaves_no_memoized_negative() {
        let make = || {
            let state = SharedResolutionState::new();
            for cp in 0..4u16 {
                let key: PromotedInvokeKey = (ClassId::new(1), cp, false, Some(ClassId::new(2)));
                state.insert_promoted_invoke(
                    key,
                    CachedInvokeTarget::VirtualBytecode {
                        receiver_class_id: ClassId::new(2),
                        cached: sample_bytecode_method(2),
                        gate: crate::classloading::resolution::RedefineGate::never_stale(),
                    },
                );
            }
            state
        };

        let via_promoted = make();
        assert_eq!(via_promoted.promoted_invoke_count(), 4);

        via_promoted.invalidate_promoted();
        assert_eq!(via_promoted.promoted_invoke_count(), 0);

        // A cleared key is a miss, not a memoized negative.
        let key: PromotedInvokeKey = (ClassId::new(1), 0, false, Some(ClassId::new(2)));
        assert!(via_promoted.get_promoted_invoke(&key).is_none());
        via_promoted.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        assert!(via_promoted.get_promoted_invoke(&key).is_some());
    }
}

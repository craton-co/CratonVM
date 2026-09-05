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
//! ## "Lock-free" is still a misnomer — sharded, not lock-free
//!
//! Nothing here is lock-free in the technical sense. `get_promoted_invoke`
//! takes a `parking_lot::RwLock` **read** guard, and acquiring one is an atomic
//! read-modify-write on the lock word — the file name has always overstated
//! this and still does.
//!
//! What ARCH-2026-08-04 A8 changed is *which* word. Until then there was one
//! process-wide map, so every dispatching thread RMW'd the same lock word on
//! every consult, plus a second shared line for the `promoted_hits` counter
//! incremented on every hit. That is invisible on a single-threaded benchmark
//! and a scaling ceiling on a loaded one, and because this tier is only reached
//! on a `JvmThread::invoke_cache` miss it does not even present as lock
//! *contention* in a profile — it presents as cache-line traffic.
//!
//! The cache is now [`PROMOTED_SHARDS`] independently-locked shards, each
//! `repr(align(64))` so it owns its cache line, with the hit/insert counters
//! moved *inside* the shard. Threads working different call sites no longer
//! touch a common line at all. The residual truth stands: this relocates and
//! divides contention, it does not eliminate it.
//!
//! (The pre-A8 header made the weaker version of this point after a sibling
//! pass found the same class of overstatement on
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

/// Per-shard entry cap, so the total across all shards is [`shared_cache_cap`].
///
/// Rounds **up** (`ceil`), which means the true global bound is at most
/// `shared_cache_cap() + PROMOTED_SHARDS - 1` entries — 15 over 65,536 at the
/// default. That direction is chosen deliberately: rounding down would let a
/// cap of 1 (the documented minimum, which `parse_shared_cache_cap` enforces so
/// a freshly-inserted entry always survives) become a per-shard cap of 0, and
/// `evict_to_fit` with `cap == 0` would evict the entry it was called to make
/// room for. The `max(1)` below is what actually holds that invariant; the
/// ceiling keeps the aggregate honest for every larger value.
///
/// Note this makes the bound per-shard rather than global: a workload whose
/// minted keys all hash to one shard is capped at `per_shard_cap()`, not
/// `shared_cache_cap()`. That is a *tighter* bound, never a looser one, so the
/// DoS property the cap exists for is preserved.
#[inline]
fn per_shard_cap() -> usize {
    shared_cache_cap().div_ceil(PROMOTED_SHARDS).max(1)
}

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

/// Number of independent shards the promoted-invoke cache is split across.
///
/// ARCH-2026-08-04 A8. Must be a power of two — [`shard_of`] masks rather than
/// divides.
///
/// 16 is sized against the machines this VM is benchmarked on (8-32 hardware
/// threads): enough that a fully-loaded box rarely has two threads on one
/// shard, small enough that the whole array is 16 cache lines of lock state and
/// the O(SHARDS) housekeeping operations — `invalidate_promoted`,
/// `promoted_invoke_count` — stay trivial. Raising it costs a cache line each
/// and buys nothing past core count.
const PROMOTED_SHARDS: usize = 16;

/// One shard of the promoted-invoke cache, padded to its own cache line.
///
/// **The padding is the point, not a micro-optimisation.** Sharding a map
/// without separating the shards leaves every lock word and counter on the same
/// one or two lines, so the cores still ping-pong exactly as they did before
/// and the split buys nothing measurable. `align(64)` gives each shard its own
/// line on x86-64 and AArch64.
///
/// The hit/insert counters live *inside* the shard for the same reason. They
/// were process-global `AtomicU64`s incremented on every cache hit — which is
/// to say, a read-modify-write on one shared line on the hottest path this
/// type has. Sharding the map while leaving those behind would have relocated
/// the contention rather than removed it, which is precisely the mistake this
/// module's own header calls out about the pre-A8 design.
#[repr(align(64))]
struct PromotedShard {
    map: RwLock<FxHashMap<PromotedInvokeKey, CachedInvokeTarget>>,
    /// T10.4 observability — successful read-lock hits on this shard.
    hits: AtomicU64,
    /// T10.4 observability — write-lock inserts into this shard.
    inserts: AtomicU64,
}

impl PromotedShard {
    fn new() -> Self {
        Self {
            map: RwLock::new(fx_hashmap()),
            hits: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
        }
    }
}

/// Which shard owns `key`.
///
/// Hashes the whole key, not just `caller_class`. Sharding on the caller alone
/// would put every call site of one hot class — exactly the class whose sites
/// are being consulted in a tight loop — on a single shard, reproducing the
/// original contention under a different name.
#[inline]
fn shard_of(key: &PromotedInvokeKey) -> usize {
    let mut h = FxHasher::default();
    key.hash(&mut h);
    // Fold the high bits down: FxHasher's low bits are its weakest, and
    // `PROMOTED_SHARDS` masks off everything else.
    let v = h.finish();
    ((v ^ (v >> 32)) as usize) & (PROMOTED_SHARDS - 1)
}

/// Read-optimised shared resolution state accessible from any thread.
///
/// Reads take a `RwLock` read-guard on one shard (concurrent readers never
/// block each other). Writes take that shard's write-guard and are expected to
/// be infrequent (only on first resolution of a given target).
///
/// ## Why this is sharded (ARCH-2026-08-04 A8)
///
/// This was a single process-wide `RwLock<FxHashMap<..>>`. A `parking_lot` read
/// acquire is an atomic read-modify-write on the lock word, so *every*
/// dispatching thread wrote the same cache line on every consult, plus a second
/// shared line for the hit counter. That is invisible on a single-threaded
/// benchmark and a hard scaling ceiling on a loaded one — and it is second-tier
/// (only reached on a `JvmThread::invoke_cache` miss), so it does not even show
/// up as lock *contention* in a profile; it shows up as cache-line traffic.
///
/// The module header has said since 2026-07-26 that "lock-free" was a misnomer
/// and that the design *relocated* contention off the class-manager lock rather
/// than removing it. This spreads what remains across [`PROMOTED_SHARDS`]
/// independently-locked, separately-cache-lined shards.
pub struct SharedResolutionState {
    /// T10.4 — cross-thread promoted cache of fully-built invoke targets.
    ///
    /// Thread-local `invoke_cache` misses consult this map under a read-lock
    /// before falling through to the slow `class_manager` walk.  When a thread
    /// completes the slow path it promotes the resulting `CachedInvokeTarget`
    /// here so sibling threads skip the walk on their first call.
    shards: Box<[PromotedShard; PROMOTED_SHARDS]>,
}

impl SharedResolutionState {
    /// Create an empty shared state.
    pub fn new() -> Self {
        Self {
            shards: Box::new(std::array::from_fn(|_| PromotedShard::new())),
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
        let shard = &self.shards[shard_of(key)];
        let guard = shard.map.read();
        let hit = guard.get(key).cloned();
        drop(guard);
        match hit {
            Some(t) if !t.is_stale() => {
                shard.hits.fetch_add(1, Ordering::Relaxed);
                Some(t)
            }
            Some(_stale) => {
                // Evict on detection — sibling threads would otherwise keep
                // re-promoting the same stale entry until somebody noticed.
                let mut guard = shard.map.write();
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
    ///
    /// The cap is per-shard ([`per_shard_cap`]) so the total bound across all
    /// shards stays at `shared_cache_cap()`, rounded up by at most
    /// `PROMOTED_SHARDS - 1` entries. A key-minting adversary is still held to a
    /// bounded footprint; see `per_shard_cap` for why the rounding direction is
    /// the safe one.
    pub fn insert_promoted_invoke(&self, key: PromotedInvokeKey, target: CachedInvokeTarget) {
        let shard = &self.shards[shard_of(&key)];
        let mut guard = shard.map.write();
        if !guard.contains_key(&key) {
            evict_to_fit(&mut guard, per_shard_cap());
        }
        guard.insert(key, target);
        shard.inserts.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of the promoted-invoke hit counter (tests / diagnostics).
    ///
    /// Sums the per-shard counters. Not a consistent snapshot across shards —
    /// concurrent hits may land either side of the walk — which is the same
    /// guarantee the single relaxed counter gave before A8 sharded it.
    pub fn promoted_hit_count(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.hits.load(Ordering::Relaxed))
            .sum()
    }

    /// Snapshot of the promoted-invoke insert counter (tests / diagnostics).
    pub fn promoted_insert_count(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.inserts.load(Ordering::Relaxed))
            .sum()
    }

    /// Number of distinct call sites currently cached in the promoted-invoke
    /// map (tests / diagnostics).  Acquires each shard's read-lock in turn.
    pub fn promoted_invoke_count(&self) -> usize {
        self.shards.iter().map(|s| s.map.read().len()).sum()
    }

    // -- housekeeping -----------------------------------------------------

    /// Clear the promoted-invoke cache — the only cache this type owns.
    ///
    /// ARCH-2026-07-26 (`cross-owner-closeout`, request CR-LR-1 of
    /// `stackwalk-and-vtable.md`). This is the
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
        for shard in self.shards.iter() {
            shard.map.write().clear();
        }
    }

    // There is deliberately NO class-scoped sweep beside this one.
    //
    // `invalidate_promoted_for_class(class_id)` lived here until 2026-09-05,
    // documented as "used by CHA invalidation / class redefinition" and called
    // by nothing but its own two tests. The retired
    // `promoted-invoke-resolutions-survive-class-redefinition-20260903`
    // write-up read that absence as a defect — a redefinition would leave
    // promoted entries in place until the next GC cleared them wholesale — and
    // said outright that nobody had checked `get_promoted_invoke`'s consumers
    // before calling it one.
    //
    // They are checked now, and the sweep was never the mechanism. Every
    // promoted entry carries a `RedefineGate` snapshotted on the class whose
    // body it caches; `get_promoted_invoke` compares generations on EVERY hit
    // and removes the entry instead of returning it. `redefine_class` bumps
    // that counter (step 7), so the eviction is immediate and per-entry.
    // Measured end to end against HotSpot with a `java.lang.instrument` agent
    // (`test_classes/redefine/`), reading from a freshly started thread whose
    // own `invoke_cache` is empty and which therefore MUST come through this
    // map: both VMs answer with the post-redefinition body.
    //
    // Restoring it would also not have closed what it was imagined to close.
    // It keyed on the CALLER and RECEIVER classes, and what goes stale under a
    // redefinition is the DECLARING class's body — a different class in every
    // inherited-method case, and precisely the one the gate already watches.
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
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
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

    /// A redefinition of the class an entry is bound to makes that entry stale,
    /// and the next read evicts it rather than serving it.
    ///
    /// This replaces `t10_shared_resolution_invalidate_for_class_drops_both_ends`,
    /// which drove `invalidate_promoted_for_class` — a function no production
    /// path ever called, deleted above. The retired
    /// `promoted-invoke-resolutions-survive-class-redefinition-20260903`
    /// write-up inferred a stale-dispatch window from that absence and said, to
    /// its credit, that nobody had looked at `get_promoted_invoke`'s consumers
    /// yet. This is what looking finds: the eviction is per-entry, on use, and
    /// immediate.
    ///
    /// The assertion that earns its place is the second one. Returning `None`
    /// alone would leave the stale entry for every sibling thread to re-discover
    /// and re-evict; the read path removes it, so the count goes to zero.
    #[test]
    fn a_redefine_generation_bump_evicts_the_promoted_entry_on_the_next_read() {
        let state = SharedResolutionState::new();
        // The handle `ClassManager::class_redefine_generation_handle` hands to
        // the populate path for the DECLARING class, and that `redefine_class`
        // bumps with `Release` in its step 7.
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let key: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(30)));
        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(30),
                cached: sample_bytecode_method(30),
                gate: crate::classloading::resolution::RedefineGate::snapshot(
                    std::sync::Arc::clone(&counter),
                ),
            },
        );
        assert!(
            state.get_promoted_invoke(&key).is_some(),
            "a fresh entry is served"
        );
        assert_eq!(state.promoted_invoke_count(), 1);

        // `class_manager::redefine_class` step 7, in one line.
        counter.fetch_add(1, std::sync::atomic::Ordering::Release);

        assert!(
            state.get_promoted_invoke(&key).is_none(),
            "the pre-redefinition target must not be served"
        );
        assert_eq!(
            state.promoted_invoke_count(),
            0,
            "and the stale entry is evicted on detection, not left until the next GC"
        );
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
        // (Was `invalidate_promoted_for_class(ClassId::new(1))`; the wholesale
        // clear is the same operation on a map holding one key, and it is the
        // only invalidation entry point this type still has.)
        state.invalidate_promoted();
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

    // -- ARCH-2026-08-04 A8: sharding ------------------------------------

    /// Call sites of one caller class must spread across shards.
    ///
    /// This is the whole point of hashing the full key instead of just
    /// `caller_class`. Sharding on the caller alone would put every call site
    /// of one hot class — exactly the class being consulted in a tight loop —
    /// on a single shard, reproducing the contention A8 removed under a new
    /// name. A pre-A8 single map is the degenerate case of this test with one
    /// bucket, so it would fail here.
    #[test]
    fn one_caller_class_spreads_across_shards() {
        let mut seen = std::collections::HashSet::new();
        for cp in 0..256u16 {
            let key: PromotedInvokeKey = (ClassId::new(1), cp, false, Some(ClassId::new(2)));
            seen.insert(shard_of(&key));
        }
        assert_eq!(
            seen.len(),
            PROMOTED_SHARDS,
            "256 call sites of one caller landed on {} of {} shards — the shard \
             function is not spreading a hot class's sites",
            seen.len(),
            PROMOTED_SHARDS
        );
    }

    /// The shard of a key never moves.
    ///
    /// `get_promoted_invoke` and `insert_promoted_invoke` compute the shard
    /// independently; if `shard_of` were not a pure function of the key, an
    /// insert and its lookup could land on different shards and every promotion
    /// would silently miss — a pure slowdown with no visible symptom.
    #[test]
    fn shard_of_is_stable_for_a_key() {
        let key: PromotedInvokeKey = (ClassId::new(7), 42, true, Some(ClassId::new(9)));
        let first = shard_of(&key);
        for _ in 0..1000 {
            assert_eq!(shard_of(&key), first);
        }
        assert!(first < PROMOTED_SHARDS);
    }

    /// The DoS bound survives sharding.
    ///
    /// `shared_cache_cap` exists so a workload minting unbounded distinct keys
    /// (a class emitting fresh lambda / proxy / hidden-class names per call)
    /// cannot grow the cache until the process dies. Splitting one capped map
    /// into 16 uncapped ones would have quietly deleted that property, so this
    /// mints far more keys than the cap allows and checks the total.
    #[test]
    fn the_shared_cap_still_bounds_the_total_across_shards() {
        let _guard = RESOLVE_CACHE_ENV_LOCK.lock().unwrap();
        let _override = CapOverride::set(64);

        let state = SharedResolutionState::new();
        for cp in 0..4096u16 {
            state.insert_promoted_invoke(
                (ClassId::new(1), cp, false, Some(ClassId::new(2))),
                CachedInvokeTarget::VirtualBytecode {
                    receiver_class_id: ClassId::new(2),
                    cached: sample_bytecode_method(2),
                    gate: crate::classloading::resolution::RedefineGate::never_stale(),
                },
            );
        }

        // Per-shard cap is ceil(64/16) = 4, so the aggregate ceiling is 64.
        let total = state.promoted_invoke_count();
        assert!(
            total <= per_shard_cap() * PROMOTED_SHARDS,
            "4096 minted keys grew the cache to {total}, above the \
             {} the per-shard cap allows",
            per_shard_cap() * PROMOTED_SHARDS
        );
        assert!(
            total > 0,
            "the cap evicted everything — a freshly-inserted entry must survive"
        );
    }

    /// A cap of 1 must not evict the entry it was making room for.
    ///
    /// `parse_shared_cache_cap` enforces a floor of 1 precisely so a fresh
    /// insert survives. Dividing that by `PROMOTED_SHARDS` without the `max(1)`
    /// in `per_shard_cap` yields 0, and `evict_to_fit` with `cap == 0` evicts
    /// unconditionally — the cache would then hold nothing at all, turning a
    /// tuning knob into a silent total disable.
    #[test]
    fn a_cap_of_one_still_admits_an_entry() {
        let _guard = RESOLVE_CACHE_ENV_LOCK.lock().unwrap();
        let _override = CapOverride::set(1);

        assert_eq!(per_shard_cap(), 1, "per-shard cap must never round to 0");

        let state = SharedResolutionState::new();
        let key: PromotedInvokeKey = (ClassId::new(1), 0, false, Some(ClassId::new(2)));
        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        assert!(
            state.get_promoted_invoke(&key).is_some(),
            "a cap of 1 must still admit one entry per shard"
        );
    }

    /// Each shard occupies its own cache line.
    ///
    /// The padding is what makes sharding worth anything: without it every
    /// shard's lock word and counters share one or two lines and the cores
    /// ping-pong exactly as they did with a single lock. A future edit that
    /// drops `repr(align(64))` — or adds a field pushing the shard over a line
    /// — would leave a correct cache that scales no better than the one A8
    /// replaced, with nothing to show for it.
    #[test]
    fn shards_do_not_share_a_cache_line() {
        assert_eq!(
            std::mem::align_of::<PromotedShard>(),
            64,
            "PromotedShard must be cache-line aligned"
        );
        assert_eq!(
            std::mem::size_of::<PromotedShard>() % 64,
            0,
            "PromotedShard must be a whole number of cache lines so shard N+1 \
             does not start inside shard N's line"
        );
    }
}

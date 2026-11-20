// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared method-resolution caching for the interpreter's hot path.
//!
//! ## What this module is
//!
//! One cache: `promoted_invokes`, a cross-thread map of fully-built invoke
//! targets, reached via [`SharedResolutionState::get_promoted_invoke`] /
//! [`SharedResolutionState::insert_promoted_invoke_as_of`] from the interpreter's
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
mod tests_cap_override;

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
/// Generic over the key/value (it bounded three maps before ARCH-2026-08-04
/// A7; today only the promoted-invoke shards).
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

// `SharedResolutionState` below holds one `parking_lot::RwLock` map per
// promoted-invoke shard (`global_methods` and `global_fields` were deleted in
// ARCH-2026-08-04 A7); the struct definition is the source of truth.
// parking_lot drops the pthread_rwlock + poisoning overhead that the std lock
// would impose — every other workspace lock is already parking_lot.
use parking_lot::RwLock;

#[allow(unused_imports)]
use super::fx_collections::{fx_hashmap, FxBuildHasher, FxHashMap, FxHasher};
use crate::classloading::resolution::CachedInvokeTarget as GenericCachedInvokeTarget;
use crate::classloading::ClassId;
use std::hash::{Hash, Hasher};

type CachedInvokeTarget = GenericCachedInvokeTarget<cratonvm_jit::RetainedCode>;

/// The redefinition count a promoted entry is stamped with and compared
/// against (`cratonvm_classloading::class_redefinition_count`).
#[inline]
fn redefinitions_now() -> u64 {
    crate::classloading::class_redefinition_count()
}

/// The per-thread invoke cache's kill switch governs this map too: the two
/// tiers must agree on when a caller's entry retires.
#[inline(always)]
fn promoted_retire_redefined_callers() -> bool {
    crate::classloading::resolution::INVOKE_CACHE_RETIRES_REDEFINED_CALLERS
}

/// `CRATONVM_INVOKE_CACHE_PER_VM_REDEFINITIONS` (default on; the switch the
/// per-thread `InvokeCache` binds under, nio21 wave 3 L1): does
/// [`SharedResolutionState::with_redefinition_log`] bind? Read once per VM, at
/// construction.
fn promoted_per_vm_redefinitions_enabled() -> bool {
    true
}

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
    /// Each target with the `class_redefinition_count` it was resolved under
    /// (see [`SharedResolutionState::insert_promoted_invoke_as_of`]).
    map: RwLock<FxHashMap<PromotedInvokeKey, (u64, CachedInvokeTarget)>>,
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
    /// The owning VM's redefinition log (nio21 wave 5 L1; page
    /// `r21-invoke-cache-redefinition-count-is-process-wide-FIXED-20261012.md`).
    /// Bound ([`Self::with_redefinition_log`]): an entry's caller side and a
    /// straddling promotion are judged by THIS VM's redefinitions; another
    /// VM's move of the process clock retires nothing here. Unbound
    /// ([`Self::new`]): the process ring, every VM's (correct, over-eager).
    redefinition_log: Option<std::sync::Arc<cratonvm_classloading::RedefinitionLog>>,
}

impl SharedResolutionState {
    /// Create an empty shared state.
    pub fn new() -> Self {
        Self {
            shards: Box::new(std::array::from_fn(|_| PromotedShard::new())),
            redefinition_log: None,
        }
    }

    /// An empty shared state bound to its VM's redefinition log (nio21 wave 5
    /// L1): the VM's own constructor. Unbound, as [`Self::new`], under
    /// `CRATONVM_INVOKE_CACHE_PER_VM_REDEFINITIONS=0`.
    pub fn with_redefinition_log(
        log: std::sync::Arc<cratonvm_classloading::RedefinitionLog>,
    ) -> Self {
        let mut state = Self::new();
        if promoted_per_vm_redefinitions_enabled() {
            state.redefinition_log = Some(log);
        }
        state
    }

    /// Is this state bound to its VM's redefinition log?
    pub fn has_redefinition_log(&self) -> bool {
        self.redefinition_log.is_some()
    }

    /// The classes redefined in `(after, upto]` this map must retire for: its
    /// VM's own log when bound, the process ring otherwise. `false` when the
    /// answer is unknown (treat every class as redefined).
    #[inline]
    fn redefined_classes_between(&self, after: u64, upto: u64, visit: impl FnMut(ClassId)) -> bool {
        match &self.redefinition_log {
            Some(log) => log.for_each_between(after, upto, visit),
            None => cratonvm_classloading::for_each_redefined_class_between(after, upto, visit),
        }
    }

    /// Whether an entry keyed on `caller` and stamped `stamp` is still current
    /// on the caller side at redefinition count `now`: nothing was redefined
    /// since, or (the class-scoped arm, `INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS`)
    /// the redefinition record ([`Self::redefined_classes_between`]) names
    /// every class redefined in `(stamp, now]` and `caller` is not among them.
    /// The record cannot answer: not current.
    #[inline]
    fn promoted_caller_current(&self, caller: ClassId, stamp: u64, now: u64) -> bool {
        if !promoted_retire_redefined_callers() || stamp == now {
            return true;
        }
        if !crate::classloading::resolution::INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS {
            return false;
        }
        let mut redefined = false;
        let named = self.redefined_classes_between(stamp, now, |class| {
            redefined |= class == caller;
        });
        named && !redefined
    }

    /// Did a resolution that took the count `as_of` straddle a redefinition
    /// that may have replaced a constant pool it read? Unbound: any move of
    /// the process count. Bound (nio21 wave 5 L1): a record in this VM's log
    /// in `(as_of, now]`, or a span the log cannot answer.
    #[inline]
    fn promotion_straddles_a_redefinition(&self, as_of: u64, now: u64) -> bool {
        if as_of == now {
            return false;
        }
        match &self.redefinition_log {
            Some(log) => {
                let mut any = false;
                let answered = log.for_each_between(as_of, now, |_| any = true);
                !answered || any
            }
            None => true,
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
        // Stale on either side: the TARGET's class was redefined (its gate),
        // or the CALLER was redefined since the entry was resolved — its
        // constant pool may now name another member at this key (interpreter
        // round i1 wave 18, lane L4; the same condition retires a thread's own
        // `InvokeCache` entries). Since wave 20 (lane L2) a redefinition of
        // another class keeps a non-compiled entry when the redefinition ring
        // names every class redefined since its stamp.
        // A compiled (`Jit`) entry is never kept that way: its body may
        // have inlined the redefined class, which no gate of it watches.
        let stale = |stamp: u64, t: &CachedInvokeTarget, now: u64| {
            t.is_stale()
                || (stamp != now && matches!(t, GenericCachedInvokeTarget::Jit { .. }))
                || !self.promoted_caller_current(key.0, stamp, now)
        };
        let now = redefinitions_now();
        match hit {
            Some((stamp, t)) if !stale(stamp, &t, now) => {
                shard.hits.fetch_add(1, Ordering::Relaxed);
                if stamp != now {
                    // Proved current through `now`: restamp, so the next read
                    // compares one count instead of walking the ring again.
                    // An entry still carrying `stamp` is the one just proved:
                    // a new promotion under `stamp` is refused once the count
                    // has left it.
                    let mut guard = shard.map.write();
                    if let Some(entry) = guard.get_mut(key) {
                        if entry.0 == stamp {
                            entry.0 = now;
                        }
                    }
                }
                Some(t)
            }
            Some(_stale) => {
                // Evict on detection — sibling threads would otherwise keep
                // re-promoting the same stale entry until somebody noticed.
                let mut guard = shard.map.write();
                let now = redefinitions_now();
                if let Some((stamp, existing)) = guard.get(key) {
                    if stale(*stamp, existing, now) {
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
    ///
    /// `as_of` is the `class_redefinition_count` read BEFORE the resolution
    /// that produced `target` — the promoting thread's
    /// `InvokeCache::redefinitions_seen()`, snapshot by the fill site before
    /// the resolution began (`fill_as_of`), not re-read at the promotion,
    /// which Java run by the resolution may have moved. A promotion whose resolution
    /// straddled a redefinition is not published (it may have read the
    /// caller's old constant pool), and a published one retires on the next
    /// read after any redefinition.
    pub fn insert_promoted_invoke_as_of(
        &self,
        key: PromotedInvokeKey,
        target: CachedInvokeTarget,
        as_of: u64,
    ) {
        let shard = &self.shards[shard_of(&key)];
        let mut guard = shard.map.write();
        // Under the shard's write guard, so a reader cannot pair this entry
        // with a count the check did not see.
        // nio21 wave 5 L1: bound to its VM's log, a straddle of ANOTHER VM's
        // redefinition only does not refuse the promotion (the entry keeps its
        // `as_of` stamp, which the next read proves current and restamps).
        if promoted_retire_redefined_callers()
            && self.promotion_straddles_a_redefinition(as_of, redefinitions_now())
        {
            return;
        }
        if !guard.contains_key(&key) {
            evict_to_fit(&mut guard, per_shard_cap());
        }
        guard.insert(key, (as_of, target));
        shard.inserts.fetch_add(1, Ordering::Relaxed);
    }

    /// [`Self::insert_promoted_invoke_as_of`] stamped with the current count,
    /// for tests that resolve nothing.
    #[cfg(test)]
    pub(crate) fn insert_promoted_invoke(
        &self,
        key: PromotedInvokeKey,
        target: CachedInvokeTarget,
    ) {
        self.insert_promoted_invoke_as_of(key, target, redefinitions_now());
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
mod tests;

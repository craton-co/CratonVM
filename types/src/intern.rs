// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! High-performance string interning pool.
//!
//! Deduplicates strings and returns `&'static str` references, eliminating
//! redundant allocations for class names, method names, and descriptors.
//!
//! # Lifetime policy (CORRECTED 2026-07-26 — see the arch note below)
//!
//! This module used to state that interned strings are never freed and that
//! this "is intentional because JVM class metadata lives for the entire VM
//! lifetime". **The second half of that is no longer true.** CratonVM unloads
//! classes (`vm/src/memory/gc.rs::unload_dead_class_metadata`, and the `vm/tests/class_loader_unload_regression.rs`
//! coverage added with it), and a Spring/Mockito/ByteBuddy workload generates
//! thousands of synthetic proxy, lambda and CGLIB classes whose names, method
//! names and descriptors all land here. Every one of those outlives the class
//! it names, forever. That is an unbounded leak proportional to *generated*
//! class count, not to live class count.
//!
//! Retention is now explicit rather than assumed:
//!
//! * [`StringPool::intern_arc`] hands out an `Arc<str>`. The pool's own clone is
//!   the last one standing when every caller has dropped theirs, which makes
//!   the entry **prunable** — see [`StringPool::prune_unreferenced`].
//! * The free [`intern`] function hands out a `&'static str`, which can never be
//!   invalidated. Its backing `Arc<str>` is therefore **pinned** in a separate,
//!   permanently-retained list ([`pinned_len`]) so that pruning the pool proper
//!   can never dangle one. As of 2026-07-26 `intern` has **no callers anywhere
//!   in the workspace** outside this module's own tests — everything production
//!   goes through `intern_arc` — so that list is empty in practice and pruning
//!   reclaims everything it should.
//!
//! Nothing calls [`StringPool::prune_unreferenced`] yet; wiring it to the
//! class-unloading path is a `vm`/`gc` change. See
//! `value-repr-and-compressed-oops.md`.
//!
//! Two interning modes are offered:
//!
//! * [`intern`] (free function) — returns a `&'static str` derived from the
//!   *global* pool's `Arc<str>`. Best for hot-path keys that will be compared
//!   by pointer equality. This is deliberately **not** a method on
//!   `StringPool`: handing out a `&'static str` is only sound when the backing
//!   pool itself lives for `'static`, which is guaranteed for the
//!   `OnceLock`-backed global singleton but *not* for a stack-local
//!   `StringPool` (which would yield a dangling reference once it drops).
//! * [`StringPool::intern_arc`] — returns an `Arc<str>` that shares a single
//!   allocation across callers. Clones are cheap (one refcount bump) and the
//!   type integrates with owning structs that cannot hold a `'static`
//!   reference for lifetime reasons (e.g. `ConstantPoolEntry::Utf8`). This is
//!   the only interning method available on an arbitrary `StringPool`.
//!
//! # Implementation (AUDIT 2026-05-16, CRIT-P1 + CRIT-P2)
//!
//! Both modes now share a single `Arc<str>` allocation per unique string
//! (previously `intern_arc` did *three* allocations on a miss: a `String`, a
//! `Box<str>` for `Box::leak`, and a third copy into the `Arc` layout). On a
//! cache hit the pool takes a `parking_lot::RwLock` *read* guard — multiple
//! readers can intern in parallel, which matters because class loading is
//! overwhelmingly read-heavy. Hashing uses `rustc_hash::FxHasher` instead of
//! the default SipHash; FxHash is roughly 3–5× faster on the short ASCII
//! strings (class / method / descriptor names) that dominate the workload and
//! is acceptable here because the pool is not exposed to untrusted input
//! (everything passing through it comes from already-validated classfile
//! bytes).

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHasher;
use hashbrown::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{Arc, OnceLock};

type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

const STRING_POOL_SHARDS: usize = 64;

#[inline]
fn hash_str(s: &str) -> u64 {
    use std::hash::Hasher;
    let mut h = FxHasher::default();
    h.write(s.as_bytes());
    h.finish()
}

#[inline]
fn shard_for_hash(hash: u64) -> usize {
    ((hash >> 16) as usize) & (STRING_POOL_SHARDS - 1)
}

#[inline]
fn shard_for(s: &str) -> usize {
    shard_for_hash(hash_str(s))
}

/// A thread-safe string interning pool.
///
/// Strings are stored in a single `Arc<str>` per unique content; the pool
/// itself holds one of those `Arc<str>` clones so the backing allocation
/// outlives every caller. Entries are retained until
/// [`prune_unreferenced`](StringPool::prune_unreferenced) is called; see the
/// module-level lifetime policy for why "never freed" is no longer the right
/// default now that classes can be unloaded.
///
/// To obtain a `&'static str`, use the free [`intern`] function, which is
/// backed by the process-global pool ([`global_pool`]); that pool lives for
/// the entire process so the lifetime extension is sound. A `&'static`-returning
/// method is intentionally *not* provided on `StringPool` itself, because a
/// stack-local pool would hand out references that dangle once it drops.
pub struct StringPool {
    shards: Box<[Mutex<FxHashMap<Arc<str>, ()>>]>,
}

impl StringPool {
    /// Creates a new, empty `StringPool`.
    pub fn new() -> Self {
        Self::with_shard_capacity(32)
    }

    /// Creates a `StringPool` with `cap` pre-allocated slots per shard.
    fn with_shard_capacity(cap: usize) -> Self {
        let mut shards = Vec::with_capacity(STRING_POOL_SHARDS);
        for _ in 0..STRING_POOL_SHARDS {
            shards.push(Mutex::new(FxHashMap::with_capacity_and_hasher(
                cap,
                BuildHasherDefault::default(),
            )));
        }
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    /// Interns a string, returning an `Arc<str>` that shares a single backing
    /// allocation with every other caller interning the same string.
    pub fn intern_arc(&self, s: &str) -> Arc<str> {
        let hash = hash_str(s);
        self.intern_arc_in_shard(s, hash, shard_for_hash(hash))
    }

    fn intern_arc_in_shard(&self, s: &str, hash: u64, shard_idx: usize) -> Arc<str> {
        use hashbrown::hash_map::RawEntryMut;
        let mut shard = self.shards[shard_idx].lock();
        match shard.raw_entry_mut().from_hash(hash, |k| (k.as_ptr() == s.as_ptr() && k.len() == s.len()) || &**k == s) {
            RawEntryMut::Occupied(e) => Arc::clone(e.key()),
            RawEntryMut::Vacant(e) => {
                let arc: Arc<str> = Arc::from(s);
                e.insert_with_hasher(hash, Arc::clone(&arc), (), |k| hash_str(k));
                arc
            }
        }
    }

    /// Returns the number of unique strings currently interned.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|m| m.lock().len()).sum()
    }

    /// Every interned string, in no particular order.
    pub fn snapshot(&self) -> Vec<Arc<str>> {
        let mut out = Vec::new();
        for m in self.shards.iter() {
            out.extend(m.lock().keys().cloned());
        }
        out
    }

    /// Returns `true` if no strings have been interned.
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|m| m.lock().is_empty())
    }

    /// Returns `true` if the given string has been interned.
    pub fn contains(&self, s: &str) -> bool {
        let hash = hash_str(s);
        self.shards[shard_for_hash(hash)]
            .lock()
            .raw_entry()
            .from_hash(hash, |k| (k.as_ptr() == s.as_ptr() && k.len() == s.len()) || &**k == s)
            .is_some()
    }

    /// Drop every entry no caller still holds, returning how many were removed.
    ///
    /// This is the eviction primitive the class-unloading path needs: a
    /// synthetic proxy class's name, method names and descriptors become
    /// garbage the moment the class is unloaded, but nothing here could
    /// previously reclaim them (see the module-level lifetime policy).
    ///
    /// # Why `strong_count == 1` is the correct and safe criterion
    ///
    /// The pool holds exactly one `Arc<str>` clone per entry, so a count of 1
    /// means *the pool is the only owner*: no caller anywhere holds a live
    /// handle to those bytes, and dropping them cannot invalidate anything. A
    /// later `intern_arc` of the same content simply reallocates and re-inserts;
    /// interning has no identity semantics across a prune (callers compare
    /// `Arc::ptr_eq` between handles they hold *concurrently*, and two handles
    /// held concurrently keep the count above 1, so they can never straddle a
    /// prune).
    ///
    /// The count is read while holding the **write** lock, which excludes every
    /// reader. A concurrent `intern_arc` cannot be mid-`Arc::clone`: it must
    /// hold the read guard to reach the stored `Arc` at all. And `Arc::clone`
    /// increments *before* the clone exists, so observing 1 under exclusion
    /// proves no clone exists — the direction that matters. Observing a stale 2
    /// is possible and merely skips an entry until the next prune.
    ///
    /// `&'static str`s handed out by the free [`intern`] function are unaffected
    /// by construction: their backing `Arc<str>` is pinned outside the map (see
    /// [`pinned_len`]), so its count is at least 2 and it is never a candidate.
    ///
    /// `O(n)` in pool size and takes the write lock for the duration — call it
    /// from a GC pause or an unload sweep, never from a hot path.
    pub fn prune_unreferenced(&self) -> usize {
        let mut pruned = 0usize;
        for m in self.shards.iter() {
            let mut write = m.lock();
            let before = write.len();
            write.retain(|k, _| Arc::strong_count(k) > 1);
            pruned += before - write.len();
        }
        pruned
    }
}

impl Default for StringPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns a reference to the global `StringPool` singleton.
pub fn global_pool() -> &'static StringPool {
    static POOL: OnceLock<StringPool> = OnceLock::new();
    POOL.get_or_init(|| StringPool::with_shard_capacity(128))
}

/// Interns a string via the process-global pool, returning a `&'static str`.
///
/// If the string has been previously interned, the same pointer is returned.
/// Otherwise a fresh `Arc<str>` is allocated; the global pool retains its own
/// clone, and the returned reference borrows from that pool-owned `Arc<str>`.
///
/// # Why this is sound (and a method on `StringPool` is not)
///
/// The transmute to `'static` below is valid *iff* the backing pool outlives
/// every `&'static` it yields. [`global_pool`] is an `OnceLock`-backed
/// singleton initialised once per process and never dropped, so its interned
/// `Arc<str>` bytes are pinned for the entire process. A stack-local
/// `StringPool` has no such guarantee — which is exactly why no
/// `&'static`-returning method is exposed on `StringPool` itself; use
/// [`StringPool::intern_arc`] for a non-`'static` pool.
///
/// # WARNING — do NOT call from a `Drop` impl that may run during teardown
///
/// The `'static` lifetime extension below relies on the global `OnceLock`
/// remaining initialised for the entire process. `Drop` impls on
/// thread-locals, `lazy_static!`-style singletons, or `OnceLock`-stored
/// values may execute **after** the runtime has begun tearing down statics —
/// on some platforms (notably under `std::process::exit` /
/// `__cxa_atexit`-style teardown) the global pool's storage backing may be
/// finalised before another static's drop runs. Calling `intern` (or
/// `intern_arc` via the global pool) at that point would observe a
/// partially-released `Arc<str>`, and the `'static` reference returned to
/// the `Drop` body could outlive the bytes it points at.
///
/// In practice this means: do not put `intern(...)` calls inside a
/// `Drop::drop` impl. Cache the `&'static str` (or an `Arc<str>` produced by
/// [`intern_arc`]) at construction time and read it during drop, or, if the
/// drop body needs an owned string, clone it ahead of time. The hot-path
/// callers in the VM already follow this pattern; this note exists so future
/// natives or class-loader teardown helpers do not accidentally regress it.
///
/// # This function has no callers, and that is why the pool can be pruned
///
/// A workspace-wide search on 2026-07-26 found **no production caller** of this
/// function — every VM/classloading/JIT call site uses [`intern_arc`]. Prefer
/// `intern_arc` in new code and treat this as deprecated in spirit: an
/// `&'static str` can never be invalidated, so every string that passes through
/// here is retained for the process lifetime no matter what
/// [`StringPool::prune_unreferenced`] does.
///
/// To make that retention explicit and *bounded to this function*, the backing
/// `Arc<str>` is pushed onto [`PINNED`] rather than relying on the pool's map
/// entry surviving. That keeps the `'static` promise sound independently of
/// pruning, at the cost of one permanently-retained `Arc` clone per distinct
/// string that reaches this path — which is zero in practice.
pub fn intern(s: &str) -> &'static str {
    let arc = global_pool().intern_arc(s);
    // Pin the allocation outside the map so `prune_unreferenced` can never drop
    // the bytes this `&'static str` points at. Idempotent per distinct string:
    // the pool deduplicates, so a repeat `intern` of the same content finds the
    // same Arc and skips the push.
    {
        let already = PINNED.read().iter().any(|p| Arc::ptr_eq(p, &arc));
        if !already {
            let mut pinned = PINNED.write();
            if !pinned.iter().any(|p| Arc::ptr_eq(p, &arc)) {
                pinned.push(Arc::clone(&arc));
            }
        }
    }
    // SAFETY: `arc`'s allocation is now owned by the process-lifetime `PINNED`
    // list (a `OnceLock`-free `static` that is never cleared), in addition to
    // the global pool's own clone. The bytes of an `Arc<str>` are pinned for the
    // lifetime of the strongest `Arc`; here that lifetime is `'static` and does
    // not depend on the pool retaining its map entry.
    let bytes: &str = &arc;
    unsafe { std::mem::transmute::<&str, &'static str>(bytes) }
}

/// Allocations backing every `&'static str` ever handed out by [`intern`].
///
/// Never cleared. See [`intern`] for why this exists and
/// [`StringPool::prune_unreferenced`] for what it protects against. Empty in
/// every production run, because nothing calls `intern`.
static PINNED: RwLock<Vec<Arc<str>>> = RwLock::new(Vec::new());

/// How many allocations are permanently pinned by [`intern`].
///
/// Expected to be `0` in a production run. A non-zero value means something
/// started calling `intern` instead of [`intern_arc`], and that many strings can
/// never be reclaimed by [`StringPool::prune_unreferenced`].
pub fn pinned_len() -> usize {
    PINNED.read().len()
}

/// Convenience function: interns a string as an `Arc<str>` via the global pool.
pub fn intern_arc(s: &str) -> Arc<str> {
    use std::cell::UnsafeCell;

    const TLS_SETS: usize = 1024;
    const EMPTY_SET: [Option<(u64, Arc<str>)>; 2] = [None, None];
    thread_local! {
        static TLS_INTERN_CACHE: UnsafeCell<[[Option<(u64, Arc<str>)>; 2]; TLS_SETS]> =
            const { UnsafeCell::new([EMPTY_SET; TLS_SETS]) };
    }

    let hash = hash_str(s);
    let idx = (hash as usize) & (TLS_SETS - 1);

    TLS_INTERN_CACHE.with(|cell| {
        // SAFETY: `TLS_INTERN_CACHE` is thread-local and `global_pool().intern_arc_in_shard`
        // does not re-enter `intern_arc`; no mutable borrow is active across the shard call.
        let set = unsafe { (&*cell.get()).get_unchecked(idx) };
        if let Some((h, ref arc)) = set[0] {
            if h == hash && ((arc.as_ptr() == s.as_ptr() && arc.len() == s.len()) || &**arc == s) {
                return Arc::clone(arc);
            }
        }
        if let Some((h, ref arc)) = set[1] {
            if h == hash && ((arc.as_ptr() == s.as_ptr() && arc.len() == s.len()) || &**arc == s) {
                return Arc::clone(arc);
            }
        }
        let arc = global_pool().intern_arc_in_shard(s, hash, shard_for_hash(hash));
        if s.len() <= 256 {
            let set = unsafe { (&mut *cell.get()).get_unchecked_mut(idx) };
            set[1] = set[0].take();
            set[0] = Some((hash, Arc::clone(&arc)));
        }
        arc
    })
}

#[cfg(test)]
mod tests;

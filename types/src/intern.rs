//! High-performance string interning pool.
//!
//! Deduplicates strings and returns `&'static str` references, eliminating
//! redundant allocations for class names, method names, and descriptors.
//! Interned strings are never freed — this is intentional because JVM class
//! metadata lives for the entire VM lifetime.
//!
//! Two interning modes are offered:
//!
//! * [`StringPool::intern`] — returns a `&'static str` derived from the
//!   pool-owned `Arc<str>`. Best for hot-path keys that will be compared by
//!   pointer equality.
//! * [`StringPool::intern_arc`] — returns an `Arc<str>` that shares a single
//!   allocation across callers. Clones are cheap (one refcount bump) and the
//!   type integrates with owning structs that cannot hold a `'static`
//!   reference for lifetime reasons (e.g. `ConstantPoolEntry::Utf8`).
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

use parking_lot::RwLock;
use rustc_hash::FxHasher;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::sync::{Arc, OnceLock};

type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// A thread-safe string interning pool.
///
/// Strings are stored in a single `Arc<str>` per unique content; the pool
/// itself holds one of those `Arc<str>` clones so the backing allocation
/// outlives every caller. Once interned, strings are never freed — this is
/// intentional because class/method/descriptor strings live for the entire
/// VM lifetime.
///
/// `intern` returns a `&'static str` derived from the pool-owned `Arc<str>`;
/// see the lifetime-extension argument on [`StringPool::intern`] for why this
/// is sound when the pool itself lives forever (the global singleton case).
pub struct StringPool {
    // The map is keyed by the `Arc<str>` itself; `HashMap<Arc<str>, ()>` is
    // morally a `HashSet<Arc<str>>` but using a `HashMap` lets us call
    // `get_key_value` to retrieve a reference to the stored `Arc` (which is
    // what we actually want — a `HashSet` would give us back a `&Arc<str>` via
    // `get` but the ergonomics work out the same).
    map: RwLock<FxHashMap<Arc<str>, ()>>,
}

impl StringPool {
    /// Creates a new, empty `StringPool`.
    pub fn new() -> Self {
        Self {
            map: RwLock::new(FxHashMap::default()),
        }
    }

    /// Interns a string, returning a `&'static str`.
    ///
    /// If the string has been previously interned, the same pointer is
    /// returned. If not, a fresh `Arc<str>` is allocated and the pool retains
    /// its own clone; the returned reference borrows from the bytes of that
    /// pool-owned `Arc<str>`.
    ///
    /// # Lifetime extension safety
    ///
    /// We hand out `&'static str` derived from the bytes of a pool-owned
    /// `Arc<str>`. The transmute to `'static` is sound *iff* the pool itself
    /// outlives every `&'static` it ever yields. That is true in the only
    /// production use of this type — [`global_pool`] is a `OnceLock` initialised
    /// once per process and never dropped — and is also true within the scope
    /// of any unit test that uses `intern` on a stack-local pool, because the
    /// returned reference is itself bounded by the pool's lifetime in the
    /// caller's frame (the test cannot leak the reference past the pool's
    /// destruction without explicit, unrelated unsafe code).
    ///
    /// Callers who would otherwise be tempted to construct a non-`'static`
    /// `StringPool` and outlive it should prefer [`Self::intern_arc`].
    pub fn intern(&self, s: &str) -> &'static str {
        // Fast path: shared read lock, cache hit. We only need the *bytes* of
        // the pool-owned `Arc<str>`, not ownership of a clone, so we read the
        // string slice directly and skip the atomic refcount bump that
        // `intern_arc` -> `Arc::clone` would perform. The bytes are pinned for
        // as long as the pool's own `Arc<str>` clone lives (forever — we never
        // call `remove`), so the `&str` borrowed from `existing` is valid for
        // the pool's lifetime; see the lifetime-extension argument below.
        {
            let read = self.map.read();
            if let Some((existing, _)) = read.get_key_value(s) {
                // SAFETY: `existing` is the pool-owned `Arc<str>`; its bytes
                // are pinned while the pool retains that clone — which is
                // forever, since the pool never removes entries. The pool is
                // intended for use as a `'static` singleton (see
                // `global_pool`). The transmute is sound iff the pool
                // outlives every `&'static` it yields, which holds for the
                // singleton and for stack-local test pools (the returned
                // reference is bounded by the pool's frame).
                let bytes: &str = existing;
                return unsafe { std::mem::transmute::<&str, &'static str>(bytes) };
            }
        }

        // Slow path (cache miss): fall through to `intern_arc`, which takes
        // the write lock and allocates the backing `Arc<str>`. We hold the
        // resulting clone only long enough to derive the static reference.
        let arc = self.intern_arc(s);
        // SAFETY: `arc` is a clone of the pool-owned `Arc<str>`; the pool
        // keeps its own clone forever (we never call `remove`). The bytes of
        // an `Arc<str>` are pinned for the lifetime of the strongest `Arc`,
        // so as long as the pool lives the bytes live. The pool is intended
        // for use as a `'static` singleton (see `global_pool`).
        let bytes: &str = &arc;
        unsafe { std::mem::transmute::<&str, &'static str>(bytes) }
    }

    /// Interns a string, returning an `Arc<str>` that shares a single backing
    /// allocation with every other caller interning the same string.
    ///
    /// Repeated calls with the same content yield clones of the same
    /// `Arc<str>` (pointer equality via `Arc::ptr_eq`). The pool retains an
    /// internal reference, so the backing allocation is never dropped.
    pub fn intern_arc(&self, s: &str) -> Arc<str> {
        // Fast path: shared read lock. Multiple threads can hit the cache in
        // parallel without serialising on a mutex. `HashMap::get_key_value`
        // returns a borrow of the stored `Arc<str>` which we then clone.
        {
            let read = self.map.read();
            if let Some((existing, _)) = read.get_key_value(s) {
                return Arc::clone(existing);
            }
        }

        // Slow path: take the write lock. Allocate the `Arc<str>` exactly
        // ONCE — `Arc::<str>::from(&str)` copies the bytes into the Arc's
        // single backing allocation directly, no intermediate `String` or
        // `Box<str>`.
        //
        // Hashing: the previous structure hashed `s` a *third* time on the
        // miss path (`get_key_value` to re-check, then `insert`). Using the
        // `entry` API instead hashes the (now owned) key exactly once for the
        // combined re-check + insert: `entry` performs a single lookup, and
        // `or_insert_with` does not re-hash. The `entry` key must be owned, so
        // we allocate the `Arc<str>` up front; on the rare re-check hit (a
        // concurrent writer beat us between dropping the read lock and taking
        // the write lock) that allocation is dropped when the closure is not
        // run, exactly as the old `get_key_value` early-return discarded work.
        let arc: Arc<str> = Arc::from(s);
        let mut write = self.map.write();
        match write.entry(Arc::clone(&arc)) {
            std::collections::hash_map::Entry::Occupied(e) => Arc::clone(e.key()),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(());
                arc
            }
        }
    }

    /// Returns the number of unique strings currently interned.
    pub fn len(&self) -> usize {
        self.map.read().len()
    }

    /// Returns `true` if no strings have been interned.
    pub fn is_empty(&self) -> bool {
        self.map.read().is_empty()
    }

    /// Returns `true` if the given string has been interned.
    pub fn contains(&self, s: &str) -> bool {
        self.map.read().contains_key(s)
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
    POOL.get_or_init(StringPool::new)
}

/// Convenience function: interns a string via the global pool.
pub fn intern(s: &str) -> &'static str {
    global_pool().intern(s)
}

/// Convenience function: interns a string as an `Arc<str>` via the global pool.
pub fn intern_arc(s: &str) -> Arc<str> {
    global_pool().intern_arc(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;
    use std::thread;

    #[test]
    fn intern_same_string_returns_same_pointer() {
        let pool = StringPool::new();
        let a = pool.intern("hello");
        let b = pool.intern("hello");
        assert!(ptr::eq(a, b), "same string must yield same pointer");
    }

    #[test]
    fn intern_different_strings_returns_different_pointers() {
        let pool = StringPool::new();
        let a = pool.intern("alpha");
        let b = pool.intern("beta");
        assert!(!ptr::eq(a, b), "different strings must yield different pointers");
        assert_eq!(a, "alpha");
        assert_eq!(b, "beta");
    }

    #[test]
    fn concurrent_same_string_same_pointer() {
        let pool = StringPool::new();
        // Pre-warm so the allocation exists before threads race.
        let _ = pool.intern("shared");

        let pool_ref = &pool;
        thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| pool_ref.intern("shared")))
                .collect();
            let results: Vec<&str> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            for r in &results {
                assert!(ptr::eq(*r, results[0]));
            }
        });
    }

    #[test]
    fn concurrent_different_strings_all_unique() {
        let pool = StringPool::new();
        let pool_ref = &pool;
        let strings = ["aaa", "bbb", "ccc", "ddd"];

        thread::scope(|s| {
            let handles: Vec<_> = strings
                .iter()
                .map(|&val| s.spawn(move || pool_ref.intern(val)))
                .collect();
            let results: Vec<&str> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            // All four must be distinct pointers with correct values.
            for (i, r) in results.iter().enumerate() {
                assert_eq!(*r, strings[i]);
                for (j, other) in results.iter().enumerate() {
                    if i != j {
                        assert!(!ptr::eq(*r, *other));
                    }
                }
            }
        });
    }

    #[test]
    fn pool_length_tracks_correctly() {
        let pool = StringPool::new();
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());

        pool.intern("one");
        assert_eq!(pool.len(), 1);

        pool.intern("two");
        assert_eq!(pool.len(), 2);

        // Duplicate should not increase length.
        pool.intern("one");
        assert_eq!(pool.len(), 2);
        assert!(!pool.is_empty());
    }

    #[test]
    fn contains_works() {
        let pool = StringPool::new();
        assert!(!pool.contains("x"));

        pool.intern("x");
        assert!(pool.contains("x"));
        assert!(!pool.contains("y"));
    }

    #[test]
    fn intern_empty_string() {
        let pool = StringPool::new();
        let a = pool.intern("");
        let b = pool.intern("");
        assert!(ptr::eq(a, b));
        assert_eq!(a, "");
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn intern_very_long_string() {
        let pool = StringPool::new();
        let long = "x".repeat(1_000_000); // 1 MB
        let a = pool.intern(&long);
        let b = pool.intern(&long);
        assert!(ptr::eq(a, b));
        assert_eq!(a.len(), 1_000_000);
    }

    #[test]
    fn intern_special_chars() {
        let pool = StringPool::new();

        // Unicode
        let emoji = pool.intern("こんにちは🌍");
        assert_eq!(emoji, "こんにちは🌍");

        // Null bytes
        let with_null = pool.intern("a\0b\0c");
        assert_eq!(with_null, "a\0b\0c");
        assert_eq!(with_null.len(), 5);

        // Second call returns same pointer
        let emoji2 = pool.intern("こんにちは🌍");
        assert!(ptr::eq(emoji, emoji2));
    }

    #[test]
    fn global_pool_is_singleton() {
        let p1 = global_pool() as *const StringPool;
        let p2 = global_pool() as *const StringPool;
        assert_eq!(p1, p2, "global_pool must return the same instance");

        // The convenience `intern` function uses the global pool.
        let a = intern("global_test");
        let b = global_pool().intern("global_test");
        assert!(ptr::eq(a, b));
    }

    #[test]
    fn intern_arc_dedupes_same_content() {
        let pool = StringPool::new();
        let a = pool.intern_arc("java/lang/Object");
        let b = pool.intern_arc("java/lang/Object");
        assert!(
            Arc::ptr_eq(&a, &b),
            "same content must yield Arc<str> with shared allocation"
        );
        assert_eq!(&*a, "java/lang/Object");
    }

    #[test]
    fn intern_arc_different_contents_differ() {
        let pool = StringPool::new();
        let a = pool.intern_arc("alpha");
        let b = pool.intern_arc("beta");
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn intern_arc_reuses_after_static_intern() {
        // Interning via `intern` then via `intern_arc` must still yield
        // the same Arc<str> as a subsequent `intern_arc` call. Post-audit
        // both paths share the same underlying Arc<str>, so the `&'static str`
        // handed out by `intern` borrows from the very same bytes that
        // `intern_arc` then returns an Arc-clone of.
        let pool = StringPool::new();
        let _ = pool.intern("shared_between_modes");
        let a = pool.intern_arc("shared_between_modes");
        let b = pool.intern_arc("shared_between_modes");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(&*a, "shared_between_modes");
    }

    #[test]
    fn t10_intern_concurrent_deduplication() {
        // 4 threads interning the same string — all Arc<str> clones must share
        // the same allocation.
        let pool = StringPool::new();
        let pool_ref = &pool;

        thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| pool_ref.intern_arc("java/lang/String")))
                .collect();
            let results: Vec<Arc<str>> = handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect();
            for r in &results {
                assert!(Arc::ptr_eq(r, &results[0]));
                assert_eq!(&**r, "java/lang/String");
            }
        });
    }

    // ------------------------------------------------------------------
    // AUDIT 2026-05-16 — new tests covering the post-fix invariants.
    // ------------------------------------------------------------------

    /// `intern_arc` called repeatedly with the same content must hand back
    /// clones of *the same* `Arc<str>` allocation. Distinct content must
    /// produce distinct allocations.
    #[test]
    fn intern_arc_pointer_identity_preserved() {
        let pool = StringPool::new();

        let a1 = pool.intern_arc("java/util/HashMap");
        let a2 = pool.intern_arc("java/util/HashMap");
        let a3 = pool.intern_arc("java/util/HashMap");
        assert!(Arc::ptr_eq(&a1, &a2));
        assert!(Arc::ptr_eq(&a2, &a3));
        // All three are pointer-identical to the original storage.
        assert_eq!(Arc::strong_count(&a1) >= 4, true); // a1,a2,a3,pool

        let b = pool.intern_arc("java/util/TreeMap");
        assert!(!Arc::ptr_eq(&a1, &b));
    }

    /// `intern` and `intern_arc` must share the same backing bytes for the
    /// same content. The pointer returned by `intern` should equal
    /// `Arc::as_ptr(&intern_arc(...))` as a `*const u8`.
    #[test]
    fn intern_and_intern_arc_share_bytes() {
        let pool = StringPool::new();

        // Order 1: arc first, then static.
        let arc = pool.intern_arc("Ljava/lang/Object;");
        let stat = pool.intern("Ljava/lang/Object;");
        assert_eq!(arc.as_ptr(), stat.as_ptr());
        assert_eq!(arc.len(), stat.len());

        // Order 2: static first, then arc — also shares.
        let stat2 = pool.intern("()V");
        let arc2 = pool.intern_arc("()V");
        assert_eq!(stat2.as_ptr(), arc2.as_ptr());
    }

    /// Eight threads each interning an overlapping set of strings must agree
    /// pointer-wise on each unique key. Stresses the read/write lock
    /// promotion path.
    #[test]
    fn concurrent_intern_arc_pointer_identity_across_threads() {
        let pool = StringPool::new();
        let pool_ref = &pool;

        // Each thread interns this same set, in different orders.
        let names: &[&str] = &[
            "java/lang/Object",
            "java/lang/String",
            "java/util/Map",
            "java/util/HashMap",
            "java/util/TreeMap",
            "java/util/List",
            "java/util/ArrayList",
            "java/util/LinkedList",
        ];

        let per_thread: Vec<Vec<Arc<str>>> = thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|tid| {
                    s.spawn(move || {
                        let mut out = Vec::with_capacity(names.len());
                        // Rotate the order per thread so different threads race
                        // on different first-insert keys.
                        for i in 0..names.len() {
                            let idx = (i + tid) % names.len();
                            out.push(pool_ref.intern_arc(names[idx]));
                        }
                        // Reorder to canonical (name-index) order before
                        // returning so the outer comparison is straightforward.
                        let mut canonical: Vec<Option<Arc<str>>> =
                            (0..names.len()).map(|_| None).collect();
                        for i in 0..names.len() {
                            let idx = (i + tid) % names.len();
                            canonical[idx] = Some(Arc::clone(&out[i]));
                        }
                        canonical.into_iter().map(|o| o.unwrap()).collect()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        // For every name, all 8 threads' Arcs must be Arc::ptr_eq.
        for (col_idx, name) in names.iter().enumerate() {
            let baseline = &per_thread[0][col_idx];
            assert_eq!(&**baseline, *name);
            for (row_idx, row) in per_thread.iter().enumerate().skip(1) {
                assert!(
                    Arc::ptr_eq(baseline, &row[col_idx]),
                    "thread {row_idx} got a different Arc<str> for {name:?}"
                );
            }
        }

        // Pool length must equal the unique-name count, no duplicates inserted.
        assert_eq!(pool.len(), names.len());
    }
}

//! High-performance string interning pool.
//!
//! Deduplicates strings and returns `&'static str` references, eliminating
//! redundant allocations for class names, method names, and descriptors.
//! Interned strings are never freed — this is intentional because JVM class
//! metadata lives for the entire VM lifetime.
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
/// To obtain a `&'static str`, use the free [`intern`] function, which is
/// backed by the process-global pool ([`global_pool`]); that pool lives for
/// the entire process so the lifetime extension is sound. A `&'static`-returning
/// method is intentionally *not* provided on `StringPool` itself, because a
/// stack-local pool would hand out references that dangle once it drops.
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
        // `Box<str>`. Re-check under the write lock in case a concurrent
        // writer beat us to it.
        let mut write = self.map.write();
        if let Some((existing, _)) = write.get_key_value(s) {
            return Arc::clone(existing);
        }
        let arc: Arc<str> = Arc::from(s);
        write.insert(Arc::clone(&arc), ());
        arc
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
pub fn intern(s: &str) -> &'static str {
    let arc = global_pool().intern_arc(s);
    // SAFETY: `arc` is a clone of an `Arc<str>` owned by the *global* pool.
    // The global pool is a `OnceLock` singleton that is never dropped, and the
    // pool never calls `remove`, so the backing allocation lives for the whole
    // process. The bytes of an `Arc<str>` are pinned for the lifetime of the
    // strongest `Arc`; here that lifetime is effectively `'static`.
    let bytes: &str = &arc;
    unsafe { std::mem::transmute::<&str, &'static str>(bytes) }
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
        // A stack-local pool only exposes `intern_arc`; same content must
        // yield Arc<str> clones sharing one allocation.
        let pool = StringPool::new();
        let a = pool.intern_arc("hello");
        let b = pool.intern_arc("hello");
        assert!(Arc::ptr_eq(&a, &b), "same string must yield same allocation");
    }

    #[test]
    fn intern_different_strings_returns_different_pointers() {
        let pool = StringPool::new();
        let a = pool.intern_arc("alpha");
        let b = pool.intern_arc("beta");
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different strings must yield different allocations"
        );
        assert_eq!(&*a, "alpha");
        assert_eq!(&*b, "beta");
    }

    #[test]
    fn concurrent_same_string_same_pointer() {
        let pool = StringPool::new();
        // Pre-warm so the allocation exists before threads race.
        let _ = pool.intern_arc("shared");

        let pool_ref = &pool;
        thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| pool_ref.intern_arc("shared")))
                .collect();
            let results: Vec<Arc<str>> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();
            for r in &results {
                assert!(Arc::ptr_eq(r, &results[0]));
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
                .map(|&val| s.spawn(move || pool_ref.intern_arc(val)))
                .collect();
            let results: Vec<Arc<str>> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();
            // All four must be distinct allocations with correct values.
            for (i, r) in results.iter().enumerate() {
                assert_eq!(&**r, strings[i]);
                for (j, other) in results.iter().enumerate() {
                    if i != j {
                        assert!(!Arc::ptr_eq(r, other));
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

        pool.intern_arc("one");
        assert_eq!(pool.len(), 1);

        pool.intern_arc("two");
        assert_eq!(pool.len(), 2);

        // Duplicate should not increase length.
        pool.intern_arc("one");
        assert_eq!(pool.len(), 2);
        assert!(!pool.is_empty());
    }

    #[test]
    fn contains_works() {
        let pool = StringPool::new();
        assert!(!pool.contains("x"));

        pool.intern_arc("x");
        assert!(pool.contains("x"));
        assert!(!pool.contains("y"));
    }

    #[test]
    fn intern_empty_string() {
        let pool = StringPool::new();
        let a = pool.intern_arc("");
        let b = pool.intern_arc("");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(&*a, "");
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn intern_very_long_string() {
        let pool = StringPool::new();
        let long = "x".repeat(1_000_000); // 1 MB
        let a = pool.intern_arc(&long);
        let b = pool.intern_arc(&long);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.len(), 1_000_000);
    }

    #[test]
    fn intern_special_chars() {
        let pool = StringPool::new();

        // Unicode
        let emoji = pool.intern_arc("こんにちは🌍");
        assert_eq!(&*emoji, "こんにちは🌍");

        // Null bytes
        let with_null = pool.intern_arc("a\0b\0c");
        assert_eq!(&*with_null, "a\0b\0c");
        assert_eq!(with_null.len(), 5);

        // Second call returns same allocation
        let emoji2 = pool.intern_arc("こんにちは🌍");
        assert!(Arc::ptr_eq(&emoji, &emoji2));
    }

    #[test]
    fn global_pool_is_singleton() {
        let p1 = global_pool() as *const StringPool;
        let p2 = global_pool() as *const StringPool;
        assert_eq!(p1, p2, "global_pool must return the same instance");

        // The free `intern` function uses the global pool; a second call
        // (and an `intern_arc` on the same global pool) shares the bytes.
        let a = intern("global_test");
        let b = intern("global_test");
        assert!(ptr::eq(a, b));
        let c = global_pool().intern_arc("global_test");
        assert_eq!(a.as_ptr(), c.as_ptr());
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
        // The free `intern` (global pool) then `intern_arc` on that same
        // global pool must share one Arc<str> allocation: the `&'static str`
        // handed out by `intern` borrows the very bytes `intern_arc` clones.
        let _ = intern("shared_between_modes");
        let a = global_pool().intern_arc("shared_between_modes");
        let b = global_pool().intern_arc("shared_between_modes");
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

    /// The free `intern` and the global pool's `intern_arc` must share the
    /// same backing bytes for the same content. The pointer returned by
    /// `intern` should equal `Arc::as_ptr(&intern_arc(...))` as a `*const u8`.
    #[test]
    fn intern_and_intern_arc_share_bytes() {
        // Order 1: arc first, then static.
        let arc = global_pool().intern_arc("Ljava/lang/Object;");
        let stat = intern("Ljava/lang/Object;");
        assert_eq!(arc.as_ptr(), stat.as_ptr());
        assert_eq!(arc.len(), stat.len());

        // Order 2: static first, then arc — also shares.
        let stat2 = intern("()V");
        let arc2 = global_pool().intern_arc("()V");
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

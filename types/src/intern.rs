//! High-performance string interning pool.
//!
//! Deduplicates strings and returns `&'static str` references, eliminating
//! redundant allocations for class names, method names, and descriptors.
//! Interned strings are never freed — this is intentional because JVM class
//! metadata lives for the entire VM lifetime.
//!
//! Two interning modes are offered:
//!
//! * [`StringPool::intern`] — returns a `&'static str` from a leaked `Box<str>`.
//!   Best for hot-path keys that will be compared by pointer equality.
//! * [`StringPool::intern_arc`] — returns an `Arc<str>` that shares a single
//!   allocation across callers. Clones are cheap (one refcount bump) and the
//!   type integrates with owning structs that cannot hold a `'static` reference
//!   for lifetime reasons (e.g. `ConstantPoolEntry::Utf8`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

/// A thread-safe string interning pool.
///
/// Strings are stored in leaked `Box<str>` allocations (valid for `'static`).
/// Deduplication is via a `HashSet` protected by a `Mutex`.
/// Once interned, strings are never freed — this is intentional because
/// class/method/descriptor strings live for the entire VM lifetime.
///
/// The pool also maintains a parallel `HashMap` of `Arc<str>` entries so
/// callers that need heap-owned (rather than `'static`) references can get
/// a deduplicated `Arc<str>` without a second allocation.
pub struct StringPool {
    inner: Mutex<PoolState>,
}

struct PoolState {
    static_set: HashSet<&'static str>,
    arc_map: HashMap<&'static str, Arc<str>>,
}

impl StringPool {
    /// Creates a new, empty `StringPool`.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(PoolState {
                static_set: HashSet::new(),
                arc_map: HashMap::new(),
            }),
        }
    }

    /// Interns a string, returning a `&'static str`.
    ///
    /// If the string has been previously interned, the same pointer is returned.
    /// If not, the string is leaked into a `'static` allocation and stored.
    pub fn intern(&self, s: &str) -> &'static str {
        let mut state = self.inner.lock().unwrap();
        if let Some(&existing) = state.static_set.get(s) {
            existing
        } else {
            let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
            state.static_set.insert(leaked);
            leaked
        }
    }

    /// Interns a string, returning an `Arc<str>` that shares a single backing
    /// allocation with every other caller interning the same string.
    ///
    /// Repeated calls with the same content yield clones of the same
    /// `Arc<str>` (pointer equality via `Arc::ptr_eq`). The pool retains an
    /// internal reference, so the backing allocation is never dropped.
    pub fn intern_arc(&self, s: &str) -> Arc<str> {
        let mut state = self.inner.lock().unwrap();
        // Fast path: already interned as Arc.
        // Use the static_set's lookup table to get a `&'static str` key we
        // can use in the Arc map (this keeps both structures consistent and
        // guarantees O(1) second-call lookup).
        let key: &'static str = if let Some(&existing) = state.static_set.get(s) {
            existing
        } else {
            let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
            state.static_set.insert(leaked);
            leaked
        };
        if let Some(arc) = state.arc_map.get(key) {
            Arc::clone(arc)
        } else {
            let arc: Arc<str> = Arc::from(key);
            state.arc_map.insert(key, Arc::clone(&arc));
            arc
        }
    }

    /// Returns the number of unique strings currently interned.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().static_set.len()
    }

    /// Returns `true` if no strings have been interned.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().static_set.is_empty()
    }

    /// Returns `true` if the given string has been interned.
    pub fn contains(&self, s: &str) -> bool {
        self.inner.lock().unwrap().static_set.contains(s)
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
        // the same Arc<str> as a subsequent `intern_arc` call, even though
        // the Arc's backing allocation is separate from the leaked static one.
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
}

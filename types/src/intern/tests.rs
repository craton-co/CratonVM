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
    assert!(
        Arc::ptr_eq(&a, &b),
        "same string must yield same allocation"
    );
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
        let results: Vec<Arc<str>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
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
        let results: Vec<Arc<str>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
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
        let results: Vec<Arc<str>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
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
    assert!(Arc::strong_count(&a1) >= 4); // a1,a2,a3,pool

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

// ------------------------------------------------------------------
// Eviction (2026-07-26) — class unloading makes "never freed" wrong.
// ------------------------------------------------------------------

/// The core eviction contract: an entry nobody holds is reclaimed, an entry
/// somebody holds is not.
#[test]
fn prune_unreferenced_drops_only_unheld_entries() {
    let pool = StringPool::new();

    // Held by the test for the whole function.
    let held = pool.intern_arc("com/example/HeldClass");
    // Dropped immediately — only the pool's own clone remains.
    pool.intern_arc("com/example/Proxy$$EnhancerBySpringCGLIB$$0001");
    pool.intern_arc("com/example/Proxy$$EnhancerBySpringCGLIB$$0002");
    assert_eq!(pool.len(), 3);

    let removed = pool.prune_unreferenced();
    assert_eq!(removed, 2, "both unheld proxy names must be reclaimed");
    assert_eq!(pool.len(), 1);
    assert!(pool.contains("com/example/HeldClass"));
    assert!(!pool.contains("com/example/Proxy$$EnhancerBySpringCGLIB$$0001"));
    // The held handle is untouched and still readable.
    assert_eq!(&*held, "com/example/HeldClass");

    // Once the last handle goes, the entry becomes reclaimable too.
    drop(held);
    assert_eq!(pool.prune_unreferenced(), 1);
    assert!(pool.is_empty());
}

/// The leak this closes, stated as a test: a workload that generates and
/// discards synthetic class names (ByteBuddy / CGLIB / lambda proxies) used
/// to grow the pool without bound. Pruning must return it to baseline.
#[test]
fn prune_reclaims_generated_class_name_churn() {
    let pool = StringPool::new();
    let permanent = pool.intern_arc("java/lang/Object");

    for i in 0..2000 {
        // Name, method name, descriptor — the three things class loading
        // interns per generated class.
        pool.intern_arc(&format!("com/example/Gen${i}"));
        pool.intern_arc(&format!("invoke${i}"));
        pool.intern_arc(&format!("(Lcom/example/Gen${i};)V"));
    }
    assert_eq!(pool.len(), 6001);

    let removed = pool.prune_unreferenced();
    assert_eq!(removed, 6000);
    assert_eq!(pool.len(), 1, "only the still-held name survives");
    assert_eq!(&*permanent, "java/lang/Object");
}

/// Re-interning after a prune must work and must not resurrect stale bytes.
#[test]
fn prune_then_reintern_is_a_fresh_allocation() {
    let pool = StringPool::new();
    let first = pool.intern_arc("java/util/HashMap");
    let first_ptr = first.as_ptr();
    drop(first);

    assert_eq!(pool.prune_unreferenced(), 1);
    assert!(!pool.contains("java/util/HashMap"));

    let second = pool.intern_arc("java/util/HashMap");
    assert_eq!(&*second, "java/util/HashMap");
    assert_eq!(pool.len(), 1);
    // Deduplication still works across the prune.
    let third = pool.intern_arc("java/util/HashMap");
    assert!(Arc::ptr_eq(&second, &third));
    let _ = first_ptr; // identity across a prune is explicitly not promised
}

/// Pruning an empty pool, and pruning twice, are both no-ops.
#[test]
fn prune_is_idempotent_and_safe_when_empty() {
    let pool = StringPool::new();
    assert_eq!(pool.prune_unreferenced(), 0);
    pool.intern_arc("x");
    assert_eq!(pool.prune_unreferenced(), 1);
    assert_eq!(pool.prune_unreferenced(), 0);
    assert!(pool.is_empty());
}

/// A `&'static str` from the free `intern` must stay valid across a prune of
/// the global pool — the pinning contract. This is the invariant that makes
/// `prune_unreferenced` sound while `intern` still exists.
#[test]
fn global_prune_cannot_dangle_a_static_intern() {
    let s: &'static str = intern("pinned_across_prune");
    assert!(pinned_len() >= 1);

    // Prune the global pool. The map entry may or may not survive (other
    // tests in this process hold handles), but the bytes must.
    let _ = global_pool().prune_unreferenced();

    // Reading through the &'static str after the prune must be safe and
    // must still yield the original content.
    assert_eq!(s, "pinned_across_prune");
    assert_eq!(s.len(), "pinned_across_prune".len());

    // Interning it again yields the same pinned allocation...
    let s2: &'static str = intern("pinned_across_prune");
    assert!(ptr::eq(s, s2));

    // ...and does not pin a second copy. Counted by content rather than by
    // `pinned_len()`, which other tests in this process also grow.
    let copies = PINNED
        .read()
        .iter()
        .filter(|p| &***p == "pinned_across_prune")
        .count();
    assert_eq!(copies, 1, "pinning must be idempotent per distinct string");
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

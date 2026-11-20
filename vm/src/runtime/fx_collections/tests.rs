use super::*;
use std::hash::Hash;

/// Helper: hash a value with FxHasher.
fn fx_hash_value<T: Hash>(val: &T) -> u64 {
    let mut h = FxHasher::default();
    val.hash(&mut h);
    h.finish()
}

#[test]
fn fxhasher_consistent_hashes() {
    let a = fx_hash_value(&42u64);
    let b = fx_hash_value(&42u64);
    assert_eq!(a, b);
    // Different values should (almost certainly) differ.
    let c = fx_hash_value(&43u64);
    assert_ne!(a, c);
}

#[test]
fn fxhashmap_basic_insert_get() {
    let mut m: FxHashMap<String, i32> = fx_hashmap();
    m.insert("hello".to_string(), 1);
    m.insert("world".to_string(), 2);
    assert_eq!(m.get("hello"), Some(&1));
    assert_eq!(m.get("world"), Some(&2));
    assert_eq!(m.get("missing"), None);
}

#[test]
fn fxhashmap_integer_keys_not_slower() {
    // Smoke test: insert many integer keys -- just verify correctness,
    // the performance benefit is structural (fewer instructions per hash).
    let mut m: FxHashMap<u64, u64> = fx_hashmap_with_capacity(10_000);
    for i in 0..10_000u64 {
        m.insert(i, i * 2);
    }
    for i in 0..10_000u64 {
        assert_eq!(m.get(&i), Some(&(i * 2)));
    }
}

#[test]
fn fxhashset_basic() {
    let mut s: FxHashSet<u32> = fx_hashset();
    s.insert(1);
    s.insert(2);
    s.insert(1);
    assert_eq!(s.len(), 2);
    assert!(s.contains(&1));
    assert!(s.contains(&2));
}

#[test]
fn resolution_cache_put_get_method() {
    let mut cache = ResolutionCache::new();
    let m = ResolvedMethod {
        class_id: 1,
        method_index: 0,
        is_static: true,
        is_native: false,
    };
    cache.put_method("java/lang/Object", "hashCode", "()I", m.clone());
    let got = cache
        .get_method("java/lang/Object", "hashCode", "()I")
        .unwrap();
    assert_eq!(*got, m);
}

#[test]
fn resolution_cache_put_get_field() {
    let mut cache = ResolutionCache::new();
    let f = ResolvedField {
        class_id: 2,
        field_index: 3,
        is_static: false,
    };
    cache.put_field("java/lang/String", "value", "[C", f.clone());
    let got = cache.get_field("java/lang/String", "value", "[C").unwrap();
    assert_eq!(*got, f);
}

#[test]
fn resolution_cache_put_get_call_site() {
    let mut cache = ResolutionCache::new();
    let cs = ResolvedCallSite {
        bootstrap_index: 0,
        target_class_id: 5,
        target_method_index: 12,
    };
    cache.put_call_site(99, cs.clone());
    let got = cache.get_call_site(99).unwrap();
    assert_eq!(*got, cs);
    assert!(cache.get_call_site(100).is_none());
}

#[test]
fn resolution_cache_clear() {
    let mut cache = ResolutionCache::new();
    cache.put_method(
        "A",
        "b",
        "()V",
        ResolvedMethod {
            class_id: 1,
            method_index: 0,
            is_static: false,
            is_native: false,
        },
    );
    cache.put_field(
        "A",
        "x",
        "I",
        ResolvedField {
            class_id: 1,
            field_index: 0,
            is_static: true,
        },
    );
    cache.put_call_site(
        1,
        ResolvedCallSite {
            bootstrap_index: 0,
            target_class_id: 1,
            target_method_index: 0,
        },
    );
    assert_eq!(cache.len(), 3);
    cache.clear();
    assert_eq!(cache.len(), 0);
    assert!(cache.is_empty());
}

#[test]
fn resolution_cache_len() {
    let mut cache = ResolutionCache::new();
    assert_eq!(cache.len(), 0);
    cache.put_method(
        "A",
        "m",
        "()V",
        ResolvedMethod {
            class_id: 0,
            method_index: 0,
            is_static: false,
            is_native: false,
        },
    );
    assert_eq!(cache.len(), 1);
    cache.put_field(
        "A",
        "f",
        "I",
        ResolvedField {
            class_id: 0,
            field_index: 0,
            is_static: false,
        },
    );
    assert_eq!(cache.len(), 2);
    cache.put_call_site(
        7,
        ResolvedCallSite {
            bootstrap_index: 0,
            target_class_id: 0,
            target_method_index: 0,
        },
    );
    assert_eq!(cache.len(), 3);
}

#[test]
fn resolution_cache_method_miss() {
    let cache = ResolutionCache::new();
    assert!(cache.get_method("X", "y", "()V").is_none());
}

#[test]
fn fxhasher_str_hashing() {
    // Verify string hashing is deterministic.
    let a = fx_hash_str("java/lang/Object");
    let b = fx_hash_str("java/lang/Object");
    assert_eq!(a, b);
    let c = fx_hash_str("java/lang/String");
    assert_ne!(a, c);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Behavioural coverage for `java.util.HashMap` natives.
//!
//! These tests drive the real registered natives (`put`, `get`, `remove`,
//! `size`, `keySet`) through a heap-backed `MockCtx` so the put/get/resize
//! cycle, the iterator state machine, and the CHM-style segment routing
//! get end-to-end exercise. Before this file the crate's only coverage of
//! HashMap was registry-completeness checks — behavioural coverage was
//! <5% (see `.claude/review-2026-05-24/native-collections.md` §2.2).

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{boxed_int, build_registry, call, new_concurrent_hashmap, new_hashmap, MockCtx};
use cratonvm_native_api::NativeContext;
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::{ClassId, Value};

const HM: &str = "java/util/HashMap";
const CHM: &str = "java/util/concurrent/ConcurrentHashMap";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const GET: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";

fn assert_null_pointer(result: cratonvm_types::error::MethodCallResult) {
    assert!(
        matches!(
            result,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { .. }
            )))
        ),
        "expected NullPointerException, got {result:?}"
    );
}

#[test]
fn empty_get_returns_null() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let key = boxed_int(&mut ctx, 42);
    let got = call(
        &reg,
        &mut ctx,
        HM,
        "get",
        GET,
        &[Value::Object(Some(hm)), key],
    )
    .unwrap();
    assert_eq!(
        got,
        Some(Value::Object(None)),
        "get() on an empty map must return null"
    );
}

#[test]
fn single_put_get_round_trip() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let k = boxed_int(&mut ctx, 7);
    let v = boxed_int(&mut ctx, 100);
    let prev = call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), k, v],
    )
    .unwrap();
    assert_eq!(
        prev,
        Some(Value::Object(None)),
        "put of new key returns null"
    );

    let got = call(
        &reg,
        &mut ctx,
        HM,
        "get",
        GET,
        &[Value::Object(Some(hm)), k],
    )
    .unwrap();
    assert_eq!(got, Some(v), "get of put key returns the put value");

    let size = call(
        &reg,
        &mut ctx,
        HM,
        "size",
        "()I",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(1)));
}

#[test]
fn lookup_invokes_equals_on_search_key_not_stored_key() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let stored = ctx.alloc_object(ClassId::new(0), 1);
    let search = ctx.alloc_object(ClassId::new(0), 1);
    let value = boxed_int(&mut ctx, 77);

    // Both hashCode() and equals(Object) return a non-zero int in this mock.
    // The two distinct keys therefore share a bucket and compare equal.
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(7)))]);
    call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), Value::Object(Some(stored)), value],
    )
    .unwrap();

    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(7))), Ok(Some(Value::Int(1)))]);
    let got = call(
        &reg,
        &mut ctx,
        HM,
        "get",
        GET,
        &[Value::Object(Some(hm)), Value::Object(Some(search))],
    )
    .unwrap();
    assert_eq!(got, Some(value));

    let equals_receiver = ctx
        .invoke_virtual_log()
        .into_iter()
        .find(|(_, method, descriptor, _)| {
            method == "equals" && descriptor == "(Ljava/lang/Object;)Z"
        })
        .map(|(receiver, _, _, _)| receiver)
        .expect("lookup must invoke equals for distinct colliding keys");
    assert_eq!(
        equals_receiver,
        search.as_ptr() as usize,
        "HashMap requires searchKey.equals(storedKey), not the reverse"
    );
}

#[test]
fn colliding_bucket_requires_full_hash_match_before_equals() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let stored = ctx.alloc_object(ClassId::new(0), 1);
    let search = ctx.alloc_object(ClassId::new(0), 1);
    let first_value = boxed_int(&mut ctx, 11);
    let second_value = boxed_int(&mut ctx, 22);

    // Spread hashes 1 and 17 both select bucket 1 in the initial 16-slot
    // table, but OpenJDK must not call equals unless the full stored hash also
    // matches. Returning true from equals makes a missing hash gate observable.
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(1)))]);
    call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[
            Value::Object(Some(hm)),
            Value::Object(Some(stored)),
            first_value,
        ],
    )
    .unwrap();

    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(17))), Ok(Some(Value::Int(1)))]);
    let got = call(
        &reg,
        &mut ctx,
        HM,
        "get",
        GET,
        &[Value::Object(Some(hm)), Value::Object(Some(search))],
    )
    .unwrap();
    assert_eq!(got, Some(Value::Object(None)));
    assert!(
        ctx.invoke_virtual_log()
            .iter()
            .all(|(_, method, _, _)| method != "equals"),
        "different full hashes in one bucket must bypass equals"
    );

    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(17))), Ok(Some(Value::Int(1)))]);
    call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[
            Value::Object(Some(hm)),
            Value::Object(Some(search)),
            second_value,
        ],
    )
    .unwrap();
    let size = call(
        &reg,
        &mut ctx,
        HM,
        "size",
        "()I",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(2)));
    assert!(
        ctx.invoke_virtual_log()
            .iter()
            .all(|(_, method, _, _)| method != "equals"),
        "put must not overwrite a colliding-bucket key with a different hash"
    );
}

#[test]
fn concurrent_hashmap_null_key_methods_throw_npe() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let chm = new_concurrent_hashmap(&reg, &mut ctx);
    let fallback = boxed_int(&mut ctx, 99);

    for (method, desc, args) in [
        (
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            vec![Value::Object(Some(chm)), Value::Object(None)],
        ),
        (
            "containsKey",
            "(Ljava/lang/Object;)Z",
            vec![Value::Object(Some(chm)), Value::Object(None)],
        ),
        (
            "getOrDefault",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            vec![Value::Object(Some(chm)), Value::Object(None), fallback],
        ),
        (
            "remove",
            "(Ljava/lang/Object;Ljava/lang/Object;)Z",
            vec![Value::Object(Some(chm)), Value::Object(None), fallback],
        ),
    ] {
        assert_null_pointer(call(&reg, &mut ctx, CHM, method, desc, &args));
    }
}

#[test]
fn n_100_distinct_keys_round_trip() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    // Build 100 (key, value) pairs and put them all.
    let mut pairs = Vec::with_capacity(100);
    for i in 0..100 {
        let k = boxed_int(&mut ctx, i);
        let v = boxed_int(&mut ctx, i * 10);
        pairs.push((k, v));
        call(
            &reg,
            &mut ctx,
            HM,
            "put",
            PUT,
            &[Value::Object(Some(hm)), k, v],
        )
        .unwrap();
    }

    // Every key should map back to its value.
    for (k, v) in &pairs {
        let got = call(
            &reg,
            &mut ctx,
            HM,
            "get",
            GET,
            &[Value::Object(Some(hm)), *k],
        )
        .unwrap();
        assert_eq!(got, Some(*v), "round-trip mismatch on key={:?}", k);
    }
    let size = call(
        &reg,
        &mut ctx,
        HM,
        "size",
        "()I",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(100)));
}

#[test]
fn overwrite_returns_old_value_and_keeps_size() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let k = boxed_int(&mut ctx, 1);
    let v1 = boxed_int(&mut ctx, 11);
    let v2 = boxed_int(&mut ctx, 22);

    let prev1 = call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), k, v1],
    )
    .unwrap();
    assert_eq!(prev1, Some(Value::Object(None)));

    let prev2 = call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), k, v2],
    )
    .unwrap();
    assert_eq!(
        prev2,
        Some(v1),
        "overwriting a key must return the previous value"
    );

    let got = call(
        &reg,
        &mut ctx,
        HM,
        "get",
        GET,
        &[Value::Object(Some(hm)), k],
    )
    .unwrap();
    assert_eq!(got, Some(v2));

    let size = call(
        &reg,
        &mut ctx,
        HM,
        "size",
        "()I",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(1)), "overwrite keeps size at 1");
}

#[test]
fn remove_then_get_returns_null() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let k = boxed_int(&mut ctx, 5);
    let v = boxed_int(&mut ctx, 50);
    call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), k, v],
    )
    .unwrap();

    let removed = call(
        &reg,
        &mut ctx,
        HM,
        "remove",
        GET,
        &[Value::Object(Some(hm)), k],
    )
    .unwrap();
    assert_eq!(removed, Some(v), "remove returns the prior value");

    let got = call(
        &reg,
        &mut ctx,
        HM,
        "get",
        GET,
        &[Value::Object(Some(hm)), k],
    )
    .unwrap();
    assert_eq!(got, Some(Value::Object(None)));

    let size = call(
        &reg,
        &mut ctx,
        HM,
        "size",
        "()I",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(0)));
}

#[test]
fn resize_after_32_puts_keeps_all_findable() {
    // The default initial capacity is 16. Inserting 32 distinct keys
    // forces at least one rehash (load factor 0.75 → resize at size 13,
    // and again past 24). This exercises the resize copy path that
    // the legacy / split-by-high-bit code in `map_resize` (line 1973)
    // walks — uncovered before this test.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let mut pairs = Vec::with_capacity(32);
    for i in 0..32 {
        let k = boxed_int(&mut ctx, i + 1000);
        let v = boxed_int(&mut ctx, i + 2000);
        pairs.push((k, v));
        call(
            &reg,
            &mut ctx,
            HM,
            "put",
            PUT,
            &[Value::Object(Some(hm)), k, v],
        )
        .unwrap();
    }
    for (k, v) in &pairs {
        let got = call(
            &reg,
            &mut ctx,
            HM,
            "get",
            GET,
            &[Value::Object(Some(hm)), *k],
        )
        .unwrap();
        assert_eq!(got, Some(*v), "post-resize lookup failed for key {:?}", k);
    }
    let size = call(
        &reg,
        &mut ctx,
        HM,
        "size",
        "()I",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(32)));
}

#[test]
fn keyset_iterator_visits_all_keys() {
    // Drive HashMap.keySet().iterator() through the registered natives,
    // walk all entries, and confirm every put key was visited exactly once.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let mut keys = Vec::with_capacity(10);
    for i in 0..10 {
        let k = boxed_int(&mut ctx, i + 100);
        let v = boxed_int(&mut ctx, i + 200);
        keys.push(k);
        call(
            &reg,
            &mut ctx,
            HM,
            "put",
            PUT,
            &[Value::Object(Some(hm)), k, v],
        )
        .unwrap();
    }

    let key_set = call(
        &reg,
        &mut ctx,
        HM,
        "keySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    let key_set_obj = match key_set {
        Some(Value::Object(Some(o))) => o,
        other => panic!("keySet returned {:?}", other),
    };

    // HashSet.iterator() — registered via the HS family.
    let iter = call(
        &reg,
        &mut ctx,
        "java/util/HashSet",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(key_set_obj))],
    )
    .unwrap();
    let iter_obj = match iter {
        Some(Value::Object(Some(o))) => o,
        other => panic!("HashSet.iterator returned {:?}", other),
    };

    // HashMap$KeyItr.hasNext/next is the synthetic iterator class shared
    // by HashSet's KeyItr fallback. Walk until hasNext returns 0.
    let mut visited = 0usize;
    loop {
        let has_next = call(
            &reg,
            &mut ctx,
            "java/util/HashMap$KeyItr",
            "hasNext",
            "()Z",
            &[Value::Object(Some(iter_obj))],
        )
        .unwrap();
        match has_next {
            Some(Value::Int(n)) if n != 0 => {}
            _ => break,
        }
        let n = call(
            &reg,
            &mut ctx,
            "java/util/HashMap$KeyItr",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(iter_obj))],
        )
        .unwrap();
        match n {
            Some(Value::Object(Some(_))) => visited += 1,
            _ => break,
        }
        if visited > 1000 {
            panic!("iterator runaway");
        }
    }
    assert_eq!(
        visited, 10,
        "iterator should visit each of the 10 keys exactly once, got {visited}"
    );
}

#[test]
fn keyset_initial_backing_retains_null_key() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let non_null_key = boxed_int(&mut ctx, 42);
    let value = boxed_int(&mut ctx, 7);

    call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), Value::Object(None), value],
    )
    .unwrap();
    call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), non_null_key, value],
    )
    .unwrap();

    let key_set = call(
        &reg,
        &mut ctx,
        HM,
        "keySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    let key_set_obj = match key_set {
        Some(Value::Object(Some(o))) => o,
        other => panic!("keySet returned {:?}", other),
    };
    let backing = match ctx.get_field(key_set_obj, 0) {
        Value::Object(Some(o)) => o,
        other => panic!("keySet backing was {:?}", other),
    };

    let has_null = call(
        &reg,
        &mut ctx,
        HM,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(backing)), Value::Object(None)],
    )
    .unwrap();
    assert_eq!(
        has_null,
        Some(Value::Int(1)),
        "fresh keySet backing must retain the legal null key"
    );

    let size = call(
        &reg,
        &mut ctx,
        HM,
        "size",
        "()I",
        &[Value::Object(Some(backing))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(2)));
}

#[test]
fn colliding_keys_iterate_in_tail_append_order_like_hotspot() {
    // HotSpot HashMap.putVal (JDK 8+) appends a new colliding key at the TAIL
    // of its bin, so iterating a single bucket yields *insertion* order. Keys
    // 0, 16, 32 all hash to bucket 0 (cap 16; Integer.hashCode == value, so the
    // low 4 bits are all zero). Inserting in that order must iterate in that
    // order — the old head-prepend reversed it to 32, 16, 0, which was the
    // dominant source of HashMap iteration-order divergence vs HotSpot.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    for &k in &[0i32, 16, 32] {
        let key = boxed_int(&mut ctx, k);
        let val = boxed_int(&mut ctx, k);
        call(
            &reg,
            &mut ctx,
            HM,
            "put",
            PUT,
            &[Value::Object(Some(hm)), key, val],
        )
        .unwrap();
    }

    let key_set = call(
        &reg,
        &mut ctx,
        HM,
        "keySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    let key_set_obj = match key_set {
        Some(Value::Object(Some(o))) => o,
        other => panic!("keySet returned {:?}", other),
    };
    let iter = call(
        &reg,
        &mut ctx,
        "java/util/HashSet",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(key_set_obj))],
    )
    .unwrap();
    let iter_obj = match iter {
        Some(Value::Object(Some(o))) => o,
        other => panic!("HashSet.iterator returned {:?}", other),
    };

    let mut order = Vec::new();
    loop {
        let has_next = call(
            &reg,
            &mut ctx,
            "java/util/HashMap$KeyItr",
            "hasNext",
            "()Z",
            &[Value::Object(Some(iter_obj))],
        )
        .unwrap();
        match has_next {
            Some(Value::Int(n)) if n != 0 => {}
            _ => break,
        }
        let n = call(
            &reg,
            &mut ctx,
            "java/util/HashMap$KeyItr",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(iter_obj))],
        )
        .unwrap();
        match n {
            Some(Value::Object(Some(o))) => match ctx.get_field(o, 0) {
                Value::Int(v) => order.push(v),
                other => panic!("key wrapper slot 0 not Int: {:?}", other),
            },
            _ => break,
        }
        if order.len() > 100 {
            panic!("iterator runaway");
        }
    }

    assert_eq!(
        order,
        vec![0, 16, 32],
        "colliding keys must iterate in insertion (tail-append) order, got {:?}",
        order
    );
}

#[test]
fn null_value_put_returns_null_and_then_get_returns_null() {
    // The CHM null-rejection bug (review §1.1 row 5) does NOT apply to
    // plain HashMap — `HashMap.put(k, null)` is legal per the Javadoc.
    // Lock in that behaviour so a future tightening of the null check
    // doesn't accidentally break HashMap as well.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let k = boxed_int(&mut ctx, 1);
    let prev = call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), k, Value::Object(None)],
    )
    .unwrap();
    assert_eq!(prev, Some(Value::Object(None)));

    let got = call(
        &reg,
        &mut ctx,
        HM,
        "get",
        GET,
        &[Value::Object(Some(hm)), k],
    )
    .unwrap();
    // A successful get-of-null-value returns null. The implementation does
    // not distinguish absent-key from mapped-to-null, which mirrors the
    // common HashMap-of-nulls anti-pattern. Treat both as null.
    assert_eq!(got, Some(Value::Object(None)));
}

#[test]
fn contains_key_distinguishes_present_from_absent() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let k = boxed_int(&mut ctx, 7);
    let absent = boxed_int(&mut ctx, 99);
    let v = boxed_int(&mut ctx, 1);
    call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), k, v],
    )
    .unwrap();

    let has_present = call(
        &reg,
        &mut ctx,
        HM,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(hm)), k],
    )
    .unwrap();
    assert_eq!(has_present, Some(Value::Int(1)));

    let has_absent = call(
        &reg,
        &mut ctx,
        HM,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(hm)), absent],
    )
    .unwrap();
    assert_eq!(has_absent, Some(Value::Int(0)));
}

const BIFUNC: &str =
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;";

// Regression: `native_map_merge` used to call `normalize_for_compare` on the
// BiFunction result, UNBOXING the returned Integer to a raw `Value::Int` before
// storing it. A raw primitive cannot live in the map's object-reference storage,
// so the value read back as `null` — `chm.merge("a",10,Integer::sum)` returned
// null instead of 11. The result must be stored BOXED, exactly as the remap
// function returned it (TreeMap's native already did this correctly).
#[test]
fn merge_present_key_stores_boxed_result_not_unboxed() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let key = boxed_int(&mut ctx, 1);
    let one = boxed_int(&mut ctx, 1);
    call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), key, one],
    )
    .unwrap();

    // The remap BiFunction returns a BOXED Integer(11).
    let eleven = boxed_int(&mut ctx, 11);
    ctx.set_invoke_virtual_result(Ok(Some(eleven)));

    let value = boxed_int(&mut ctx, 10);
    let bifn = boxed_int(&mut ctx, 0); // dummy non-null function receiver
    let ret = call(
        &reg,
        &mut ctx,
        HM,
        "merge",
        BIFUNC,
        &[Value::Object(Some(hm)), key, value, bifn],
    )
    .unwrap();
    assert_eq!(ret, Some(eleven), "merge returns the boxed remap result");

    // The stored value must be retrievable as the boxed Integer — NOT null.
    let got = call(
        &reg,
        &mut ctx,
        HM,
        "get",
        GET,
        &[Value::Object(Some(hm)), key],
    )
    .unwrap();
    assert_eq!(
        got,
        Some(eleven),
        "merge must store the BOXED result; unboxing made get() return null"
    );
}

// Same regression for `compute` (shared the `normalize_for_compare` bug).
#[test]
fn compute_present_key_stores_boxed_result_not_unboxed() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let key = boxed_int(&mut ctx, 1);
    let one = boxed_int(&mut ctx, 1);
    call(
        &reg,
        &mut ctx,
        HM,
        "put",
        PUT,
        &[Value::Object(Some(hm)), key, one],
    )
    .unwrap();

    let twelve = boxed_int(&mut ctx, 12);
    ctx.set_invoke_virtual_result(Ok(Some(twelve)));

    let bifn = boxed_int(&mut ctx, 0);
    let ret = call(
        &reg,
        &mut ctx,
        HM,
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        &[Value::Object(Some(hm)), key, bifn],
    )
    .unwrap();
    assert_eq!(ret, Some(twelve), "compute returns the boxed remap result");

    let got = call(
        &reg,
        &mut ctx,
        HM,
        "get",
        GET,
        &[Value::Object(Some(hm)), key],
    )
    .unwrap();
    assert_eq!(
        got,
        Some(twelve),
        "compute must store the BOXED result; unboxing made get() return null"
    );
}

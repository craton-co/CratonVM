// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! gc-common w9-d — `common-w8b-native-factories-still-single-attempt`.
//!
//! The registered growth doors (`ArrayList.add` / `add(int,E)` /
//! `ArrayList(int)` / `HashMap.put`) now answer a refused allocation with one
//! reclaim-and-retry, while the `pub` Rust doors other crates call directly
//! (`native_al_add`) keep their single no-GC attempt.
//!
//! `MockCtx::refuse_allocations(n, helps)` refuses the next `n` FALLIBLE array
//! allocations and, when `helps`, models the reclaim as a MOVING collection:
//! every pinned root is relocated and its old address unmapped. So a native
//! that kept a bare local across the reclaim reads and writes nothing
//! afterwards, which these tests see as a lost element, a wrong size or a
//! missing entry.
//!
//! Each call pins its object arguments first, as `safe_native_call` does: the
//! funnel keeps the arguments alive and remaps its pins, but it does not
//! rewrite the native's own copies.
//!
//! gc-common w10-g extends the ladder to the page's caller-sized residue:
//! `toArray` (list, set, `(T[])`), `trimToSize`, `ArrayList(Collection)`,
//! `Arrays.copyOf`, `Hashtable(int)`, the `LinkedHashMap` table and the
//! `ArrayDeque` grow. The `pub` `native_al_to_array` keeps one attempt.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{
    boxed_int, build_registry, call, new_arraylist, new_hashmap, new_linked_hashmap, MockCtx,
};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ClassId, ObjectRef, Value};

const AL: &str = "java/util/ArrayList";
const HM: &str = "java/util/HashMap";
const LHM: &str = "java/util/LinkedHashMap";
const HT: &str = "java/util/Hashtable";
const HS: &str = "java/util/HashSet";
const AD: &str = "java/util/ArrayDeque";
const TO_ARRAY: &str = "()[Ljava/lang/Object;";
const TO_ARRAY_TYPED: &str = "([Ljava/lang/Object;)[Ljava/lang/Object;";
const ADD: &str = "(Ljava/lang/Object;)Z";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const GET: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";

fn is_oom(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError { .. }))
    )
}

/// Call a registered native the way the funnel does: pin every object
/// argument for the duration of the call, release the frame afterwards.
fn call_pinned(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    method: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    let frame = ctx.pin_depth();
    for v in args {
        if let Value::Object(Some(o)) = v {
            ctx.pin_native_root(*o);
        }
    }
    let result = call(reg, ctx, class, method, desc, args);
    ctx.unpin_native_roots(frame);
    result
}

fn int_of(ctx: &MockCtx, v: Value) -> Option<i32> {
    match v {
        Value::Object(Some(o)) => match ctx.get_field(o, 0) {
            Value::Int(i) => Some(i),
            _ => None,
        },
        _ => None,
    }
}

fn list_size(reg: &NativeMethodRegistry, ctx: &mut MockCtx, list: ObjectRef) -> i32 {
    match call(reg, ctx, AL, "size", "()I", &[Value::Object(Some(list))]).unwrap() {
        Some(Value::Int(n)) => n,
        other => panic!("size() returned {other:?}"),
    }
}

fn list_get(reg: &NativeMethodRegistry, ctx: &mut MockCtx, list: ObjectRef, i: i32) -> Option<i32> {
    let v = call(
        reg,
        ctx,
        AL,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(i)],
    )
    .unwrap()
    .unwrap_or(Value::Object(None));
    int_of(ctx, v)
}

/// `ArrayList.add` grows through a reclaim that MOVES the receiver, the old
/// backing array and the element: every element and the size survive, so
/// `al_ensure_capacity` and `al_add_impl` used only refreshed references.
#[test]
fn arraylist_add_grows_through_a_moving_reclaim() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = new_arraylist(&reg, &mut ctx);
    let floor = ctx.pin_depth();
    let list_pin = ctx.pin_native_root(list);
    let mut reclaims = 0;
    for i in 0..40 {
        let elem = boxed_int(&mut ctx, i);
        let cur = ctx.read_native_pin(list_pin, list);
        // Armed before every add; only a grow consumes it (the spare-capacity
        // path does not allocate), and the next arm overwrites a leftover.
        ctx.refuse_allocations(1, true);
        let r = call_pinned(
            &reg,
            &mut ctx,
            AL,
            "add",
            ADD,
            &[Value::Object(Some(cur)), elem],
        );
        reclaims += ctx.reclaims();
        assert_eq!(r.unwrap(), Some(Value::Int(1)), "add #{i}");
    }
    ctx.refuse_allocations(0, false);
    assert!(reclaims >= 1, "no ArrayList grow took the reclaim ladder");

    let list = ctx.read_native_pin(list_pin, list);
    assert_eq!(
        list_size(&reg, &mut ctx, list),
        40,
        "size lost across a moving grow"
    );
    for i in 0..40 {
        assert_eq!(list_get(&reg, &mut ctx, list, i), Some(i), "element {i}");
    }
    ctx.unpin_native_roots(list_pin);
    assert_eq!(ctx.pin_depth(), floor, "pins leaked");
}

/// A refused RETRY falls through to the historical single-attempt allocator
/// (which the mock always serves): the add still succeeds, after exactly one
/// reclaim, with everything refreshed across it.
#[test]
fn arraylist_add_falls_through_to_the_historical_allocator_after_a_refused_retry() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = new_arraylist(&reg, &mut ctx);
    let list_pin = ctx.pin_native_root(list);
    // Fill the default capacity without refusals, then force one grow.
    for i in 0..10 {
        let elem = boxed_int(&mut ctx, i);
        let cur = ctx.read_native_pin(list_pin, list);
        call_pinned(
            &reg,
            &mut ctx,
            AL,
            "add",
            ADD,
            &[Value::Object(Some(cur)), elem],
        )
        .unwrap();
    }
    let elem = boxed_int(&mut ctx, 10);
    let cur = ctx.read_native_pin(list_pin, list);
    ctx.refuse_allocations(2, true);
    call_pinned(
        &reg,
        &mut ctx,
        AL,
        "add",
        ADD,
        &[Value::Object(Some(cur)), elem],
    )
    .unwrap();
    assert_eq!(ctx.reclaims(), 1, "one reclaim, no spinning");
    assert_eq!(ctx.pending_refusals(), 0, "the retry was attempted");
    ctx.refuse_allocations(0, false);

    let list = ctx.read_native_pin(list_pin, list);
    assert_eq!(list_size(&reg, &mut ctx, list), 11);
    for i in 0..11 {
        assert_eq!(list_get(&reg, &mut ctx, list, i), Some(i), "element {i}");
    }
    ctx.unpin_native_roots(list_pin);
}

/// `native_al_add` -- the `pub` door `native-builtins` calls as a plain Rust
/// function while holding its own bare references -- never reclaims: its grow
/// is the historical single attempt, so a queued refusal is never consumed.
#[test]
fn the_direct_rust_door_never_reclaims() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = new_arraylist(&reg, &mut ctx);
    ctx.refuse_allocations(5, true);
    for i in 0..40 {
        let elem = boxed_int(&mut ctx, i);
        cratonvm_native_collections::native_al_add(&mut ctx, &[Value::Object(Some(list)), elem])
            .unwrap();
    }
    assert_eq!(
        ctx.reclaims(),
        0,
        "a direct Rust caller must not be collected under"
    );
    assert_eq!(
        ctx.pending_refusals(),
        5,
        "the single-attempt grow took no fallible door"
    );
    ctx.refuse_allocations(0, false);
    assert_eq!(list_size(&reg, &mut ctx, list), 40);
}

/// `ArrayList(int)`: the backing array lands on the RELOCATED receiver. Proved
/// by the next 100 adds needing no grow at all (a queued refusal survives).
#[test]
fn arraylist_capacity_constructor_reclaims_and_installs_on_the_moved_receiver() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let cid = ctx.ensure_class_initialized(AL).unwrap();
    let list = ctx.alloc_object(cid, 4);
    let list_pin = ctx.pin_native_root(list);
    ctx.refuse_allocations(1, true);
    call_pinned(
        &reg,
        &mut ctx,
        AL,
        "<init>",
        "(I)V",
        &[Value::Object(Some(list)), Value::Int(100)],
    )
    .unwrap();
    assert_eq!(ctx.reclaims(), 1);
    let list = ctx.read_native_pin(list_pin, list);

    ctx.refuse_allocations(1, false);
    for i in 0..100 {
        let elem = boxed_int(&mut ctx, i);
        let cur = ctx.read_native_pin(list_pin, list);
        call_pinned(
            &reg,
            &mut ctx,
            AL,
            "add",
            ADD,
            &[Value::Object(Some(cur)), elem],
        )
        .unwrap();
    }
    assert_eq!(
        ctx.pending_refusals(),
        1,
        "an add grew: the capacity-100 array was not installed on the moved receiver"
    );
    ctx.refuse_allocations(0, false);
    let list = ctx.read_native_pin(list_pin, list);
    assert_eq!(list_size(&reg, &mut ctx, list), 100);
    ctx.unpin_native_roots(list_pin);
}

/// Both attempts refused: `ArrayList(int)` keeps its historical catchable
/// `OutOfMemoryError`, after exactly one reclaim, and leaves no pin behind.
#[test]
fn arraylist_capacity_constructor_raises_oome_when_the_retry_is_refused() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let cid = ctx.ensure_class_initialized(AL).unwrap();
    let list = ctx.alloc_object(cid, 4);
    let floor = ctx.pin_depth();
    ctx.refuse_allocations(2, true);
    let err = call_pinned(
        &reg,
        &mut ctx,
        AL,
        "<init>",
        "(I)V",
        &[Value::Object(Some(list)), Value::Int(64)],
    )
    .unwrap_err();
    assert!(is_oom(&err), "expected OutOfMemoryError, got {err:?}");
    assert_eq!(ctx.reclaims(), 1);
    assert_eq!(ctx.pin_depth(), floor, "pins leaked on the OOME path");
    ctx.refuse_allocations(0, false);
}

/// A request past `SOFT_MAX_ARRAY_LENGTH` can never be served, so it is not
/// worth a collection: HotSpot throws at once, and so does this.
#[test]
fn an_unservable_length_does_not_collect() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let cid = ctx.ensure_class_initialized(AL).unwrap();
    let list = ctx.alloc_object(cid, 4);
    ctx.refuse_allocations(1, true);
    let err = call_pinned(
        &reg,
        &mut ctx,
        AL,
        "<init>",
        "(I)V",
        &[Value::Object(Some(list)), Value::Int(i32::MAX)],
    )
    .unwrap_err();
    assert!(is_oom(&err), "expected OutOfMemoryError, got {err:?}");
    assert_eq!(
        ctx.reclaims(),
        0,
        "Integer.MAX_VALUE must not trigger a collection"
    );
    ctx.refuse_allocations(0, false);
}

/// A `java.lang.Long` key: the int-keyed overlay declines it, so every put
/// takes the node path and its `map_resize`.
fn boxed_long(ctx: &mut MockCtx, v: i64) -> ObjectRef {
    let cid = ctx.ensure_class_initialized("java/lang/Long").unwrap();
    let obj = ctx.alloc_object(cid, 1);
    ctx.set_field(obj, 0, Value::Long(v));
    obj
}

/// `HashMap.put`'s table allocation (the lazy first table and every doubling)
/// goes through the reclaim ladder, and the moving reclaim loses no entry: the
/// put window's pins cover the receiver, the key and the value.
#[test]
fn hashmap_put_resizes_through_a_moving_reclaim() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let map = new_hashmap(&reg, &mut ctx);
    let floor = ctx.pin_depth();
    let map_pin = ctx.pin_native_root(map);
    // The test's own roots for the keys, so `get` can use their current
    // addresses (a moved key is found by reference identity or by value).
    let mut key_pins = Vec::new();
    let mut keys = Vec::new();
    let mut reclaims = 0;
    for i in 0..30 {
        let key = boxed_long(&mut ctx, i as i64 * 7919);
        key_pins.push(ctx.pin_native_root(key));
        keys.push(key);
        let value = boxed_int(&mut ctx, i * 10);
        let cur = ctx.read_native_pin(map_pin, map);
        let key = ctx.read_native_pin(key_pins[i as usize], key);
        ctx.refuse_allocations(1, true);
        call_pinned(
            &reg,
            &mut ctx,
            HM,
            "put",
            PUT,
            &[Value::Object(Some(cur)), Value::Object(Some(key)), value],
        )
        .unwrap();
        reclaims += ctx.reclaims();
    }
    ctx.refuse_allocations(0, false);
    assert!(reclaims >= 1, "no HashMap resize took the reclaim ladder");

    let map = ctx.read_native_pin(map_pin, map);
    let size = call(
        &reg,
        &mut ctx,
        HM,
        "size",
        "()I",
        &[Value::Object(Some(map))],
    )
    .unwrap();
    assert_eq!(
        size,
        Some(Value::Int(30)),
        "entries lost across a moving resize"
    );
    for i in 0..30 {
        let key = ctx.read_native_pin(key_pins[i], keys[i]);
        let got = call(
            &reg,
            &mut ctx,
            HM,
            "get",
            GET,
            &[Value::Object(Some(map)), Value::Object(Some(key))],
        )
        .unwrap()
        .unwrap_or(Value::Object(None));
        assert_eq!(int_of(&ctx, got), Some(i as i32 * 10), "key #{i}");
    }
    ctx.unpin_native_roots(map_pin);
    assert_eq!(ctx.pin_depth(), floor, "pins leaked");
}

// ---------------------------------------------------------------------------
// gc-common w10-g -- residue item 2 of the page: the caller-sized sites
// (`toArray`, `trimToSize`, `Hashtable(int)`, the `LinkedHashMap` table, the
// `ArrayDeque` grow, `Arrays.copyOf`, `ArrayList(Collection)`).
// ---------------------------------------------------------------------------

fn array_of(v: Option<Value>) -> ObjectRef {
    match v {
        Some(Value::Object(Some(a))) => a,
        other => panic!("expected an array, got {other:?}"),
    }
}

/// The boxed ints of `arr`, in slot order (`None` for a slot that does not
/// hold a live `Integer` -- a stale pre-move address reads as `None`).
fn ints_of(ctx: &MockCtx, arr: ObjectRef) -> Vec<Option<i32>> {
    (0..ctx.array_length(arr))
        .map(|i| int_of(ctx, ctx.get_array_element(arr, i)))
        .collect()
}

/// An `ArrayList` of `0..n`, pinned by the caller (the returned handle).
fn filled_arraylist(reg: &NativeMethodRegistry, ctx: &mut MockCtx, n: i32) -> (ObjectRef, usize) {
    let list = new_arraylist(reg, ctx);
    let pin = ctx.pin_native_root(list);
    for i in 0..n {
        let elem = boxed_int(ctx, i);
        let cur = ctx.read_native_pin(pin, list);
        call_pinned(reg, ctx, AL, "add", ADD, &[Value::Object(Some(cur)), elem]).unwrap();
    }
    (list, pin)
}

/// `ArrayList.toArray()` through its registered door: a refused result array
/// takes one MOVING reclaim, and every element lands in the array at its
/// post-move address.
#[test]
fn arraylist_to_array_reclaims_and_stores_the_moved_elements() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let (list, pin) = filled_arraylist(&reg, &mut ctx, 6);
    let cur = ctx.read_native_pin(pin, list);
    ctx.refuse_allocations(1, true);
    let arr = call_pinned(
        &reg,
        &mut ctx,
        AL,
        "toArray",
        TO_ARRAY,
        &[Value::Object(Some(cur))],
    )
    .unwrap();
    assert_eq!(ctx.reclaims(), 1, "toArray() took the reclaim ladder once");
    ctx.refuse_allocations(0, false);
    let arr = array_of(arr);
    assert_eq!(
        ints_of(&ctx, arr),
        (0..6).map(Some).collect::<Vec<_>>(),
        "an element was stored at its pre-move address"
    );
    ctx.unpin_native_roots(pin);
}

/// `native_al_to_array` -- the `pub` door four `native-builtins` sites call as
/// a plain Rust function -- keeps its single no-GC attempt.
#[test]
fn the_direct_rust_to_array_door_never_reclaims() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let (list, pin) = filled_arraylist(&reg, &mut ctx, 3);
    let cur = ctx.read_native_pin(pin, list);
    ctx.refuse_allocations(2, true);
    let arr =
        cratonvm_native_collections::native_al_to_array(&mut ctx, &[Value::Object(Some(cur))])
            .unwrap();
    assert_eq!(
        ctx.reclaims(),
        0,
        "a direct Rust caller must not be collected under"
    );
    assert_eq!(ctx.pending_refusals(), 2, "no fallible door was taken");
    ctx.refuse_allocations(0, false);
    assert_eq!(
        ints_of(&ctx, array_of(arr)),
        vec![Some(0), Some(1), Some(2)]
    );
    ctx.unpin_native_roots(pin);
}

/// `ArrayList.trimToSize()`: the trimmed buffer is allocated under a moving
/// reclaim and installed on the MOVED receiver, with every element.
#[test]
fn arraylist_trim_to_size_reclaims_and_keeps_every_element() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    // 11 elements: the default 10 grew once, so there is slack to trim.
    let (list, pin) = filled_arraylist(&reg, &mut ctx, 11);
    let cur = ctx.read_native_pin(pin, list);
    ctx.refuse_allocations(1, true);
    call_pinned(
        &reg,
        &mut ctx,
        AL,
        "trimToSize",
        "()V",
        &[Value::Object(Some(cur))],
    )
    .unwrap();
    assert_eq!(ctx.reclaims(), 1);
    ctx.refuse_allocations(0, false);
    let list = ctx.read_native_pin(pin, list);
    assert_eq!(list_size(&reg, &mut ctx, list), 11);
    for i in 0..11 {
        assert_eq!(list_get(&reg, &mut ctx, list, i), Some(i), "element {i}");
    }
    ctx.unpin_native_roots(pin);
}

/// `ArrayList(Collection)`: the copy's buffer is allocated under a moving
/// reclaim that moves the receiver and the source; the copy lands on the
/// moved receiver with every element.
#[test]
fn arraylist_copy_constructor_reclaims_onto_the_moved_receiver() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let (source, source_pin) = filled_arraylist(&reg, &mut ctx, 12);
    let cid = ctx.ensure_class_initialized(AL).unwrap();
    let copy = ctx.alloc_object(cid, 4);
    let copy_pin = ctx.pin_native_root(copy);
    let src = ctx.read_native_pin(source_pin, source);
    ctx.refuse_allocations(1, true);
    call_pinned(
        &reg,
        &mut ctx,
        AL,
        "<init>",
        "(Ljava/util/Collection;)V",
        &[Value::Object(Some(copy)), Value::Object(Some(src))],
    )
    .unwrap();
    assert_eq!(ctx.reclaims(), 1);
    ctx.refuse_allocations(0, false);
    let copy = ctx.read_native_pin(copy_pin, copy);
    assert_eq!(
        list_size(&reg, &mut ctx, copy),
        12,
        "the copy landed on a stale receiver"
    );
    for i in 0..12 {
        assert_eq!(list_get(&reg, &mut ctx, copy, i), Some(i), "element {i}");
    }
    ctx.unpin_native_roots(source_pin);
}

/// `Arrays.copyOf(Object[], int)`: the result is allocated under a moving
/// reclaim that moves the source array; the copy reads the MOVED source.
#[test]
fn arrays_copy_of_reclaims_and_copies_from_the_moved_source() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let src = ctx.new_ref_array(ClassId::new(0), 3);
    for i in 0..3 {
        let v = boxed_int(&mut ctx, i as i32);
        ctx.set_array_element(src, i, v);
    }
    ctx.refuse_allocations(1, true);
    let out = call_pinned(
        &reg,
        &mut ctx,
        "java/util/Arrays",
        "copyOf",
        "([Ljava/lang/Object;I)[Ljava/lang/Object;",
        &[Value::Object(Some(src)), Value::Int(5)],
    )
    .unwrap();
    assert_eq!(ctx.reclaims(), 1);
    ctx.refuse_allocations(0, false);
    assert_eq!(
        ints_of(&ctx, array_of(out)),
        vec![Some(0), Some(1), Some(2), None, None]
    );
}

/// `LinkedHashMap.put`'s table (the lazy first table and every doubling)
/// goes through the reclaim ladder from the registered door, and the moving
/// reclaim loses no entry.
#[test]
fn linked_hashmap_put_resizes_through_a_moving_reclaim() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let map = new_linked_hashmap(&reg, &mut ctx, false);
    let floor = ctx.pin_depth();
    let map_pin = ctx.pin_native_root(map);
    let mut key_pins = Vec::new();
    let mut keys = Vec::new();
    let mut reclaims = 0;
    for i in 0..30 {
        let key = boxed_long(&mut ctx, i as i64 * 7919);
        key_pins.push(ctx.pin_native_root(key));
        keys.push(key);
        let value = boxed_int(&mut ctx, i * 10);
        let cur = ctx.read_native_pin(map_pin, map);
        let key = ctx.read_native_pin(key_pins[i as usize], key);
        ctx.refuse_allocations(1, true);
        call_pinned(
            &reg,
            &mut ctx,
            LHM,
            "put",
            PUT,
            &[Value::Object(Some(cur)), Value::Object(Some(key)), value],
        )
        .unwrap();
        reclaims += ctx.reclaims();
    }
    ctx.refuse_allocations(0, false);
    assert!(
        reclaims >= 1,
        "no LinkedHashMap table grow took the reclaim ladder"
    );

    let map = ctx.read_native_pin(map_pin, map);
    let size = call(
        &reg,
        &mut ctx,
        LHM,
        "size",
        "()I",
        &[Value::Object(Some(map))],
    )
    .unwrap();
    assert_eq!(
        size,
        Some(Value::Int(30)),
        "entries lost across a moving resize"
    );
    for i in 0..30 {
        let key = ctx.read_native_pin(key_pins[i], keys[i]);
        let got = call(
            &reg,
            &mut ctx,
            LHM,
            "get",
            GET,
            &[Value::Object(Some(map)), Value::Object(Some(key))],
        )
        .unwrap()
        .unwrap_or(Value::Object(None));
        assert_eq!(int_of(&ctx, got), Some(i as i32 * 10), "key #{i}");
    }
    ctx.unpin_native_roots(map_pin);
    assert_eq!(ctx.pin_depth(), floor, "pins leaked");
}

/// `Hashtable(int)`: the requested-size table is allocated under a moving
/// reclaim and installed on the MOVED receiver (the constructor's `this` was
/// a bare argument copy until w10-g). Three refusals: the default table the
/// constructor builds first may take one without reclaiming; the sized table
/// takes one, reclaims exactly once, and its retry takes or finds the rest.
#[test]
fn hashtable_capacity_constructor_installs_the_table_on_the_moved_receiver() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let cid = ctx.ensure_class_initialized(HT).unwrap();
    let table = ctx.alloc_object(cid, 8);
    let pin = ctx.pin_native_root(table);
    ctx.refuse_allocations(3, true);
    call_pinned(
        &reg,
        &mut ctx,
        HT,
        "<init>",
        "(I)V",
        &[Value::Object(Some(table)), Value::Int(64)],
    )
    .unwrap();
    assert_eq!(ctx.reclaims(), 1);
    ctx.refuse_allocations(0, false);
    let table = ctx.read_native_pin(pin, table);
    // Slot 0 is the native map layout's bucket array (`MAP_FIELD_BUCKETS`).
    let buckets = match ctx.get_field(table, 0) {
        Value::Object(Some(b)) => b,
        other => panic!("no bucket array on the moved receiver: {other:?}"),
    };
    assert_eq!(
        ctx.array_length(buckets),
        64,
        "the sized table was installed on the pre-move receiver"
    );
    ctx.unpin_native_roots(pin);
}

/// A `HashSet` of `0..n`, pinned by the caller (the returned handle).
fn filled_hashset(reg: &NativeMethodRegistry, ctx: &mut MockCtx, n: i32) -> (ObjectRef, usize) {
    let cid = ctx.ensure_class_initialized(HS).unwrap();
    let set = ctx.alloc_object(cid, 4);
    let pin = ctx.pin_native_root(set);
    call_pinned(reg, ctx, HS, "<init>", "()V", &[Value::Object(Some(set))]).unwrap();
    for i in 0..n {
        let elem = boxed_int(ctx, i);
        let cur = ctx.read_native_pin(pin, set);
        call_pinned(reg, ctx, HS, "add", ADD, &[Value::Object(Some(cur)), elem]).unwrap();
    }
    (set, pin)
}

/// `HashSet.toArray()` and `toArray(T[])`: the result is allocated under a
/// moving reclaim that moves the set and its backing map, and the elements
/// are re-collected through the moved references (the receiver was a bare
/// copy read after the allocation until w10-g).
#[test]
fn hashset_to_array_reclaims_and_recollects_through_the_moved_set() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let (set, pin) = filled_hashset(&reg, &mut ctx, 5);
    for typed in [false, true] {
        let cur = ctx.read_native_pin(pin, set);
        let out = if typed {
            let template = ctx.new_ref_array(ClassId::new(0), 0);
            ctx.refuse_allocations(1, true);
            call_pinned(
                &reg,
                &mut ctx,
                HS,
                "toArray",
                TO_ARRAY_TYPED,
                &[Value::Object(Some(cur)), Value::Object(Some(template))],
            )
        } else {
            ctx.refuse_allocations(1, true);
            call_pinned(
                &reg,
                &mut ctx,
                HS,
                "toArray",
                TO_ARRAY,
                &[Value::Object(Some(cur))],
            )
        }
        .unwrap();
        assert_eq!(ctx.reclaims(), 1, "typed={typed}");
        ctx.refuse_allocations(0, false);
        let mut got = ints_of(&ctx, array_of(out));
        got.sort();
        assert_eq!(got, (0..5).map(Some).collect::<Vec<_>>(), "typed={typed}");
    }
    ctx.unpin_native_roots(pin);
}

/// `ArrayDeque.addLast` grows its ring buffer under a moving reclaim; every
/// element survives, in order.
#[test]
fn arraydeque_add_last_grows_through_a_moving_reclaim() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let cid = ctx.ensure_class_initialized(AD).unwrap();
    let deque = ctx.alloc_object(cid, 4);
    let pin = ctx.pin_native_root(deque);
    call_pinned(
        &reg,
        &mut ctx,
        AD,
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();
    let mut reclaims = 0;
    for i in 0..40 {
        let elem = boxed_int(&mut ctx, i);
        let cur = ctx.read_native_pin(pin, deque);
        ctx.refuse_allocations(1, true);
        call_pinned(
            &reg,
            &mut ctx,
            AD,
            "addLast",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(cur)), elem],
        )
        .unwrap();
        reclaims += ctx.reclaims();
    }
    ctx.refuse_allocations(0, false);
    assert!(reclaims >= 1, "no ArrayDeque grow took the reclaim ladder");
    for i in 0..40 {
        let cur = ctx.read_native_pin(pin, deque);
        let head = call(
            &reg,
            &mut ctx,
            AD,
            "removeFirst",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(cur))],
        )
        .unwrap()
        .unwrap_or(Value::Object(None));
        assert_eq!(int_of(&ctx, head), Some(i), "element {i}");
    }
    ctx.unpin_native_roots(pin);
}

// ---------------------------------------------------------------------------
// gc-common w11-c -- `handoff-w10g-direct-rust-growth-doors` Part B (the `pub`
// reclaiming twins `native-builtins` now calls) and the page's
// `tm_ensure_capacity` residue (`TreeMap.put` / `putIfAbsent` grow).
// ---------------------------------------------------------------------------

/// `native_al_add_entry_pub`, called the way a `native-builtins` caller does
/// after w11-c: as a plain Rust function, with no funnel pins, holding only
/// its own pin on the list. The door pins its receiver and element itself, so
/// a MOVING reclaim loses no element and the caller's pin sees the moved list.
#[test]
fn the_pub_reclaiming_add_twin_grows_through_a_moving_reclaim() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = new_arraylist(&reg, &mut ctx);
    let floor = ctx.pin_depth();
    let list_pin = ctx.pin_native_root(list);
    let mut reclaims = 0;
    for i in 0..40 {
        let elem = boxed_int(&mut ctx, i);
        let cur = ctx.read_native_pin(list_pin, list);
        ctx.refuse_allocations(1, true);
        let r = cratonvm_native_collections::native_al_add_entry_pub(
            &mut ctx,
            &[Value::Object(Some(cur)), elem],
        );
        reclaims += ctx.reclaims();
        assert_eq!(r.unwrap(), Some(Value::Int(1)), "add #{i}");
    }
    ctx.refuse_allocations(0, false);
    assert!(
        reclaims >= 1,
        "no grow through the pub twin took the reclaim ladder"
    );
    let list = ctx.read_native_pin(list_pin, list);
    assert_eq!(
        list_size(&reg, &mut ctx, list),
        40,
        "size lost across a moving grow"
    );
    for i in 0..40 {
        assert_eq!(list_get(&reg, &mut ctx, list, i), Some(i), "element {i}");
    }
    ctx.unpin_native_roots(list_pin);
    assert_eq!(ctx.pin_depth(), floor, "pins leaked");
}

/// `native_al_add_at_entry_pub` (`ListIterator.add`'s door in
/// `native-builtins`): every insert at the head survives the moving reclaims,
/// so the list reads back reversed.
#[test]
fn the_pub_reclaiming_add_at_twin_grows_through_a_moving_reclaim() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = new_arraylist(&reg, &mut ctx);
    let floor = ctx.pin_depth();
    let list_pin = ctx.pin_native_root(list);
    let mut reclaims = 0;
    for i in 0..40 {
        let elem = boxed_int(&mut ctx, i);
        let cur = ctx.read_native_pin(list_pin, list);
        ctx.refuse_allocations(1, true);
        cratonvm_native_collections::native_al_add_at_entry_pub(
            &mut ctx,
            &[Value::Object(Some(cur)), Value::Int(0), elem],
        )
        .unwrap();
        reclaims += ctx.reclaims();
    }
    ctx.refuse_allocations(0, false);
    assert!(reclaims >= 1, "no add(int, E) grow took the reclaim ladder");
    let list = ctx.read_native_pin(list_pin, list);
    assert_eq!(list_size(&reg, &mut ctx, list), 40);
    for i in 0..40 {
        assert_eq!(list_get(&reg, &mut ctx, list, i), Some(39 - i), "slot {i}");
    }
    ctx.unpin_native_roots(list_pin);
    assert_eq!(ctx.pin_depth(), floor, "pins leaked");
}

/// `native_al_to_array_entry_pub` (the `getHandlers` / listener-array tail
/// calls in `native-builtins`): a refused result array takes one moving
/// reclaim, and every element lands in the fresh array.
#[test]
fn the_pub_reclaiming_to_array_twin_reclaims_and_stores_the_moved_elements() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let (list, pin) = filled_arraylist(&reg, &mut ctx, 6);
    let cur = ctx.read_native_pin(pin, list);
    ctx.refuse_allocations(1, true);
    let arr = cratonvm_native_collections::native_al_to_array_entry_pub(
        &mut ctx,
        &[Value::Object(Some(cur))],
    )
    .unwrap();
    assert_eq!(
        ctx.reclaims(),
        1,
        "the pub twin took the reclaim ladder once"
    );
    ctx.refuse_allocations(0, false);
    assert_eq!(
        ints_of(&ctx, array_of(arr)),
        (0..6).map(Some).collect::<Vec<_>>(),
        "an element was stored at its pre-move address"
    );
    ctx.unpin_native_roots(pin);
}

const TM: &str = "java/util/TreeMap";

/// A `TreeMap` ordered by the mock's `test/LiquibaseTieComparator` (orders by
/// slot 0, never answers 0), so every key takes the ARRAY path and its
/// `tm_ensure_capacity` grow rather than the comparator-less `BTreeMap` mode.
fn new_comparator_treemap(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let tm_cid = ctx.ensure_class_initialized(TM).unwrap();
    let tm = ctx.alloc_object(tm_cid, 4);
    let cmp_cid = ctx
        .ensure_class_initialized("test/LiquibaseTieComparator")
        .unwrap();
    let cmp = ctx.alloc_object(cmp_cid, 0);
    call(
        reg,
        ctx,
        TM,
        "<init>",
        "(Ljava/util/Comparator;)V",
        &[Value::Object(Some(tm)), Value::Object(Some(cmp))],
    )
    .unwrap();
    tm
}

/// A key the tie comparator orders by `order`.
fn ordered_key(ctx: &mut MockCtx, order: i32) -> Value {
    let cid = ctx.ensure_class_initialized("test/W11cOrderedKey").unwrap();
    let key = ctx.alloc_object(cid, 1);
    ctx.set_field(key, 0, Value::Int(order));
    Value::Object(Some(key))
}

/// Every `(key, value)` pair `TreeMap.forEach` hands its action, as slot-0
/// ints, in visiting order.
fn treemap_pairs(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    tm: ObjectRef,
) -> Vec<(Option<i32>, Option<i32>)> {
    let action_cid = ctx.ensure_class_initialized("test/W11cBiConsumer").unwrap();
    let action = ctx.alloc_object(action_cid, 0);
    ctx.clear_invoke_virtual_log();
    call(
        reg,
        ctx,
        TM,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        &[Value::Object(Some(tm)), Value::Object(Some(action))],
    )
    .unwrap();
    let log = ctx.invoke_virtual_log();
    let heap: &MockCtx = ctx;
    log.into_iter()
        .filter(|(_, method, _, _)| method == "accept")
        .map(|(_, _, _, args)| {
            let k = args.first().copied().unwrap_or(Value::Object(None));
            let v = args.get(1).copied().unwrap_or(Value::Object(None));
            (int_of(heap, k), int_of(heap, v))
        })
        .collect()
}

fn treemap_size(reg: &NativeMethodRegistry, ctx: &mut MockCtx, tm: ObjectRef) -> Option<Value> {
    call(reg, ctx, TM, "size", "()I", &[Value::Object(Some(tm))]).unwrap()
}

fn expected_pairs(n: i32) -> Vec<(Option<i32>, Option<i32>)> {
    (0..n).map(|i| (Some(i), Some(i * 10))).collect()
}

/// The registered `TreeMap.put` (`native_tm_put_entry`): the backing-array
/// grow (`tm_ensure_capacity`) takes the reclaim ladder, and the moving
/// reclaim loses no pair and no size -- the put window's pins cover the
/// receiver, the old array, the key and the value.
#[test]
fn treemap_put_grows_through_a_moving_reclaim() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_comparator_treemap(&reg, &mut ctx);
    let floor = ctx.pin_depth();
    let tm_pin = ctx.pin_native_root(tm);
    let mut reclaims = 0;
    for i in 0..40 {
        let key = ordered_key(&mut ctx, i);
        let value = boxed_int(&mut ctx, i * 10);
        let cur = ctx.read_native_pin(tm_pin, tm);
        ctx.refuse_allocations(1, true);
        call_pinned(
            &reg,
            &mut ctx,
            TM,
            "put",
            PUT,
            &[Value::Object(Some(cur)), key, value],
        )
        .unwrap();
        reclaims += ctx.reclaims();
    }
    ctx.refuse_allocations(0, false);
    assert!(reclaims >= 1, "no TreeMap grow took the reclaim ladder");
    let tm = ctx.read_native_pin(tm_pin, tm);
    assert_eq!(treemap_size(&reg, &mut ctx, tm), Some(Value::Int(40)));
    assert_eq!(treemap_pairs(&reg, &mut ctx, tm), expected_pairs(40));
    ctx.unpin_native_roots(tm_pin);
    assert_eq!(ctx.pin_depth(), floor, "pins leaked");
}

/// `Map.put`'s registered door routes a `TreeMap` receiver to the TreeMap put
/// WITH its own reclaiming policy (the route used to drop it).
#[test]
fn map_put_on_a_treemap_receiver_keeps_the_reclaiming_policy() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_comparator_treemap(&reg, &mut ctx);
    let tm_pin = ctx.pin_native_root(tm);
    let mut reclaims = 0;
    for i in 0..40 {
        let key = ordered_key(&mut ctx, i);
        let value = boxed_int(&mut ctx, i * 10);
        let cur = ctx.read_native_pin(tm_pin, tm);
        ctx.refuse_allocations(1, true);
        call_pinned(
            &reg,
            &mut ctx,
            "java/util/Map",
            "put",
            PUT,
            &[Value::Object(Some(cur)), key, value],
        )
        .unwrap();
        reclaims += ctx.reclaims();
    }
    ctx.refuse_allocations(0, false);
    assert!(
        reclaims >= 1,
        "Map.put on a TreeMap never took the reclaim ladder"
    );
    let tm = ctx.read_native_pin(tm_pin, tm);
    assert_eq!(treemap_pairs(&reg, &mut ctx, tm), expected_pairs(40));
    ctx.unpin_native_roots(tm_pin);
}

/// `native_map_put_pub` -- still the single-attempt door for a Rust caller
/// that holds bare references -- keeps one no-GC attempt on a `TreeMap`
/// receiver too: a queued refusal is never consumed.
#[test]
fn the_direct_rust_map_put_door_never_reclaims_on_a_treemap() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_comparator_treemap(&reg, &mut ctx);
    ctx.refuse_allocations(5, true);
    for i in 0..40 {
        let key = ordered_key(&mut ctx, i);
        let value = boxed_int(&mut ctx, i * 10);
        cratonvm_native_collections::native_map_put_pub(
            &mut ctx,
            &[Value::Object(Some(tm)), key, value],
        )
        .unwrap();
    }
    assert_eq!(
        ctx.reclaims(),
        0,
        "a direct Rust caller must not be collected under"
    );
    assert_eq!(
        ctx.pending_refusals(),
        5,
        "the single-attempt grow took no fallible door"
    );
    ctx.refuse_allocations(0, false);
    assert_eq!(treemap_pairs(&reg, &mut ctx, tm), expected_pairs(40));
}

/// `TreeMap.putIfAbsent` grows under a moving reclaim and stores the new size
/// on the MOVED receiver. Before w11-c its insert arm kept `this` as a bare
/// copy across `tm_ensure_capacity` and wrote `size` through it.
#[test]
fn treemap_put_if_absent_stores_the_size_on_the_moved_receiver() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_comparator_treemap(&reg, &mut ctx);
    let floor = ctx.pin_depth();
    let tm_pin = ctx.pin_native_root(tm);
    let mut reclaims = 0;
    for i in 0..40 {
        let key = ordered_key(&mut ctx, i);
        let value = boxed_int(&mut ctx, i * 10);
        let cur = ctx.read_native_pin(tm_pin, tm);
        ctx.refuse_allocations(1, true);
        let prev = call_pinned(
            &reg,
            &mut ctx,
            TM,
            "putIfAbsent",
            PUT,
            &[Value::Object(Some(cur)), key, value],
        )
        .unwrap();
        assert_eq!(
            prev,
            Some(Value::Object(None)),
            "putIfAbsent #{i} found a key"
        );
        reclaims += ctx.reclaims();
    }
    ctx.refuse_allocations(0, false);
    assert!(reclaims >= 1, "no putIfAbsent grow took the reclaim ladder");
    let tm = ctx.read_native_pin(tm_pin, tm);
    assert_eq!(
        treemap_size(&reg, &mut ctx, tm),
        Some(Value::Int(40)),
        "size written through a pre-move receiver"
    );
    assert_eq!(treemap_pairs(&reg, &mut ctx, tm), expected_pairs(40));
    ctx.unpin_native_roots(tm_pin);
    assert_eq!(ctx.pin_depth(), floor, "pins leaked");
}

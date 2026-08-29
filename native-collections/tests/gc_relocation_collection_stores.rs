// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! HIB-MAPRESIZE-STALE.1 — a collection native must keep every heap reference
//! it holds in a bare Rust local rooted across each allocation it makes.
//!
//! `MockCtx::set_relocate_pins_on_alloc(true)` models the worst-case moving
//! young collector: EVERY allocation relocates every pinned root and unmaps the
//! pre-move address. A native that captured a receiver, a backing array or a
//! chain cursor before an allocation and reused it afterwards therefore reads
//! and writes nothing at all here, which shows up as lost state — exactly the
//! failure the real VM turns into `gen_heap::set_field: out-of-bounds field
//! write dropped index=3 num_slots=0` and, eventually, a SIGSEGV.
//!
//! The tests below pin the receiver from the *caller* side first. That is what
//! `safe_native_call` does for a native's object arguments: it keeps the object
//! ALIVE and remaps the pin, but it does not rewrite the native's own local
//! copy. So a test that still sees correct state proves the native re-read its
//! local through a pin, not merely that the object survived.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{boxed_int, build_registry, call, MockCtx};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ObjectRef, Value};

const AL: &str = "java/util/ArrayList";
const HM: &str = "java/util/HashMap";
const HS: &str = "java/util/HashSet";
const CHM: &str = "java/util/concurrent/ConcurrentHashMap";
const LHM: &str = "java/util/LinkedHashMap";
const AD: &str = "java/util/ArrayDeque";

/// Construct `class` with its no-arg native `<init>` while relocation is ON, so
/// the constructor itself has to survive the bucket/backing allocation it makes.
fn construct_under_relocation(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    fields: usize,
) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    let obj = ctx.alloc_object(cid, fields);
    let pin = ctx.pin_native_root(obj);
    ctx.set_relocate_pins_on_alloc(true);
    call(
        reg,
        ctx,
        class,
        "<init>",
        "()V",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    ctx.set_relocate_pins_on_alloc(false);
    ctx.read_native_pin(pin, obj)
}

/// Fill `map` with `n` integer-keyed entries, relocating on every allocation,
/// re-reading the receiver through its caller pin between operations.
fn fill_map_under_relocation(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    map: ObjectRef,
    n: i32,
) -> ObjectRef {
    let pin = ctx.pin_native_root(map);
    ctx.set_relocate_pins_on_alloc(true);
    let mut cur = map;
    for i in 0..n {
        let key = boxed_int(ctx, i);
        let value = boxed_int(ctx, i * 10);
        cur = ctx.read_native_pin(pin, cur);
        call(
            reg,
            ctx,
            class,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(cur)), key, value],
        )
        .unwrap();
    }
    ctx.set_relocate_pins_on_alloc(false);
    ctx.read_native_pin(pin, cur)
}

fn get_int(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    map: ObjectRef,
    key_val: i32,
) -> Option<i32> {
    let key = boxed_int(ctx, key_val);
    let got = call(
        reg,
        ctx,
        class,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(map)), key],
    )
    .unwrap();
    match got {
        Some(Value::Object(Some(o))) => match ctx.get_field(o, 0) {
            Value::Int(v) => Some(v),
            _ => None,
        },
        _ => None,
    }
}

fn map_size(reg: &NativeMethodRegistry, ctx: &mut MockCtx, class: &str, map: ObjectRef) -> i32 {
    match call(reg, ctx, class, "size", "()I", &[Value::Object(Some(map))]).unwrap() {
        Some(Value::Int(n)) => n,
        other => panic!("size() returned {other:?}"),
    }
}

/// The `set_field` that publishes the bucket table must target the map's
/// post-move address.
///
/// **Retargeted 2026-08-22, and the coverage is stronger for it.** This test
/// used to assert the table was non-null straight after `<init>`. `29028bb9e`
/// made a JAVA-constructed `HashMap` allocate its table LAZILY, which is what
/// HotSpot does — `new HashMap<>()` leaves `table == null` until the first put,
/// with the pending capacity parked in `threshold` — so the old assertion was
/// asserting a divergence. The nine INTERNAL callers of `native_map_init` stay
/// eager (`map_init_eager`), so this is a change to the Java-constructor path
/// only.
///
/// **What this no longer covers, stated rather than quietly dropped.** MEASURED
/// after the retarget: one put under relocation gives `size() == 1` and a
/// readable value, but EVERY object field of the map is still `Object(None)`.
/// The entries live in the native side table; the Java `table` field is a
/// materialised VIEW that nothing on the `<init>`-then-`put` path builds any
/// more. So there is no `set_field` at this point to catch publishing through a
/// stale receiver, and the assertion that used to catch it has no site here.
///
/// The relocation-safety of the lazy path IS still asserted below, through the
/// natives — which is how every other test in this file states it. What is
/// owed is a test at whatever materialises `table` for a real-layout reader
/// (`H23`'s work), because that is where the field publish moved to. Filed here
/// rather than left as a silently weaker test.
#[test]
fn hashmap_lazy_table_publishes_buckets_after_relocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let map = construct_under_relocation(&reg, &mut ctx, HM, 6);
    assert!(
        !matches!(ctx.get_field(map, 0), Value::Object(Some(_))),
        "HashMap.<init> allocated a bucket table eagerly; HotSpot leaves \
         table == null until the first put (29028bb9e)"
    );
    // One put, under relocation, re-reading the receiver through its caller pin.
    // A native that kept a pre-move copy of `this` writes its entry nowhere, so
    // the size and the read-back below are what catch it.
    let map = fill_map_under_relocation(&reg, &mut ctx, HM, map, 1);
    assert_eq!(
        map_size(&reg, &mut ctx, HM, map),
        1,
        "the first put into a LAZY table was lost across a relocation"
    );
    assert_eq!(
        get_int(&reg, &mut ctx, HM, map, 0),
        Some(0),
        "the entry stored into the lazily-materialised table is not readable back"
    );
}

/// Every entry must survive the resizes that happen while the table grows —
/// `map_resize_inner` reallocates the table and then republishes it.
#[test]
fn hashmap_survives_resize_under_relocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let map = construct_under_relocation(&reg, &mut ctx, HM, 6);
    let map = fill_map_under_relocation(&reg, &mut ctx, HM, map, 64);
    assert_eq!(map_size(&reg, &mut ctx, HM, map), 64);
    for i in 0..64 {
        assert_eq!(
            get_int(&reg, &mut ctx, HM, map, i),
            Some(i * 10),
            "HashMap lost key {i} across a relocating resize"
        );
    }
}

/// `lhm_resize` rehashes by walking the insertion-order list read out of the
/// map — it read `head` through the pre-allocation address of `this`, so every
/// entry inserted before the first growth was silently dropped (and the walk
/// wrote `LinkedHashMap$Node.next` through whatever occupied the old slot).
#[test]
fn linked_hashmap_survives_resize_under_relocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let map = construct_under_relocation(&reg, &mut ctx, LHM, 8);
    let map = fill_map_under_relocation(&reg, &mut ctx, LHM, map, 64);
    assert_eq!(map_size(&reg, &mut ctx, LHM, map), 64);
    for i in 0..64 {
        assert_eq!(
            get_int(&reg, &mut ctx, LHM, map, i),
            Some(i * 10),
            "LinkedHashMap lost key {i} across a relocating resize"
        );
    }
}

/// `native_chm_contains_key` dispatched the key's `hashCode()` and then read
/// the segments array through the pre-call address of the receiver, reporting
/// ABSENT for keys the map holds.
#[test]
fn chm_contains_key_agrees_with_get_under_relocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let map = construct_under_relocation(&reg, &mut ctx, CHM, 4);
    let map = fill_map_under_relocation(&reg, &mut ctx, CHM, map, 40);

    let pin = ctx.pin_native_root(map);
    ctx.set_relocate_pins_on_alloc(true);
    let mut cur = map;
    let mut missing = Vec::new();
    for i in 0..40 {
        let key = boxed_int(&mut ctx, i);
        cur = ctx.read_native_pin(pin, cur);
        let present = call(
            &reg,
            &mut ctx,
            CHM,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(cur)), key],
        )
        .unwrap();
        if !matches!(present, Some(Value::Int(1))) {
            missing.push(i);
        }
    }
    ctx.set_relocate_pins_on_alloc(false);
    assert!(
        missing.is_empty(),
        "ConcurrentHashMap.containsKey reported absent for present keys {missing:?}"
    );
}

/// `native_hs_init` allocates the backing map (which itself allocates a bucket
/// table) before storing it into the set.
#[test]
fn hashset_add_and_contains_under_relocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let set = construct_under_relocation(&reg, &mut ctx, HS, 4);
    assert!(
        matches!(ctx.get_field(set, 0), Value::Object(Some(_))),
        "HashSet.<init> published its backing map through a stale receiver"
    );

    let pin = ctx.pin_native_root(set);
    ctx.set_relocate_pins_on_alloc(true);
    let mut cur = set;
    for i in 0..48 {
        let elem = boxed_int(&mut ctx, i);
        cur = ctx.read_native_pin(pin, cur);
        call(
            &reg,
            &mut ctx,
            HS,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(cur)), elem],
        )
        .unwrap();
    }
    ctx.set_relocate_pins_on_alloc(false);
    let set = ctx.read_native_pin(pin, cur);
    assert_eq!(
        call(
            &reg,
            &mut ctx,
            HS,
            "size",
            "()I",
            &[Value::Object(Some(set))]
        )
        .unwrap(),
        Some(Value::Int(48))
    );
}

/// `native_al_init` and the growth path both allocate the element buffer.
#[test]
fn arraylist_grows_under_relocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = construct_under_relocation(&reg, &mut ctx, AL, 4);

    let pin = ctx.pin_native_root(list);
    ctx.set_relocate_pins_on_alloc(true);
    let mut cur = list;
    for i in 0..64 {
        let elem = boxed_int(&mut ctx, i);
        cur = ctx.read_native_pin(pin, cur);
        call(
            &reg,
            &mut ctx,
            AL,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(cur)), elem],
        )
        .unwrap();
    }
    ctx.set_relocate_pins_on_alloc(false);
    let list = ctx.read_native_pin(pin, cur);
    assert_eq!(
        call(
            &reg,
            &mut ctx,
            AL,
            "size",
            "()I",
            &[Value::Object(Some(list))]
        )
        .unwrap(),
        Some(Value::Int(64))
    );
}

/// `ad_ensure_capacity` reallocates the ring buffer and republishes it into the
/// deque, copying the live window across from the old one.
#[test]
fn arraydeque_grows_under_relocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let deque = construct_under_relocation(&reg, &mut ctx, AD, 4);

    let pin = ctx.pin_native_root(deque);
    ctx.set_relocate_pins_on_alloc(true);
    let mut cur = deque;
    for i in 0..64 {
        let elem = boxed_int(&mut ctx, i);
        cur = ctx.read_native_pin(pin, cur);
        call(
            &reg,
            &mut ctx,
            AD,
            "addLast",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(cur)), elem],
        )
        .unwrap();
    }
    ctx.set_relocate_pins_on_alloc(false);
    let deque = ctx.read_native_pin(pin, cur);
    assert_eq!(
        call(
            &reg,
            &mut ctx,
            AD,
            "size",
            "()I",
            &[Value::Object(Some(deque))]
        )
        .unwrap(),
        Some(Value::Int(64))
    );
}

/// `native_lhm_entry_set` builds the view with N+2 allocations in a loop while
/// holding the map, the set, the view backing and every collected key/value in
/// bare Rust locals.
#[test]
fn linked_hashmap_entry_set_builds_under_relocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let map = construct_under_relocation(&reg, &mut ctx, LHM, 8);
    let map = fill_map_under_relocation(&reg, &mut ctx, LHM, map, 24);

    let pin = ctx.pin_native_root(map);
    ctx.set_relocate_pins_on_alloc(true);
    let set = match call(
        &reg,
        &mut ctx,
        LHM,
        "entrySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(map))],
    )
    .unwrap()
    {
        Some(Value::Object(Some(set))) => set,
        other => panic!("expected an entrySet, got {other:?}"),
    };
    ctx.set_relocate_pins_on_alloc(false);
    let _map = ctx.read_native_pin(pin, map);

    assert_eq!(
        call(
            &reg,
            &mut ctx,
            "java/util/HashSet",
            "size",
            "()I",
            &[Value::Object(Some(set))]
        )
        .unwrap(),
        Some(Value::Int(24)),
        "entrySet lost entries built across relocating allocations"
    );
}

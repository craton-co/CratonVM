// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! `LinkedHashMap` nodes must BE the real JDK node class.
//!
//! `lhm_alloc_node` used to mint `java/util/LinkedHashMap$Node`, a class the
//! real JDK does not have — its nested node type is
//! `java/util/LinkedHashMap$Entry` (`javap -p --module java.base
//! java.util.LinkedHashMap$Entry`: `class java.util.LinkedHashMap$Entry<K,V>
//! extends java.util.HashMap$Node<K,V>`). The field LAYOUT was already the real
//! one; only the class IDENTITY was invented.
//!
//! That mismatch is what turned the Kafka embedded-KRaft defect from silent
//! state divergence into a loud `ClassCastException:
//! java.util.LinkedHashMap$Node cannot be cast to java.util.LinkedHashMap$Entry`
//! (`LinkedHashMap.afterNodeRemoval`'s first statement is
//! `(LinkedHashMap.Entry<K,V>) e`), and it is why the eldest-entry hook had to
//! hand `removeEldestEntry` a copied `AbstractMap$SimpleImmutableEntry` — the
//! invented class declares no methods at all, so `eldest.getKey()` was a
//! `NoSuchMethodError`. See
//! `kafka-embedded-kraft-boundport-listeners-distinct-classcastexception-20260804-FIXED.md`.
//!
//! These are name-and-shape guards, not end-to-end ones: `MockCtx` invents a
//! `ClassId` for any name asked of it, so what they actually pin down is which
//! name the allocator asks for and which object the hook hands over. The
//! end-to-end witness against a real JDK is `probes/LinkedHashMapNodeProbe.java`,
//! which runs on HotSpot as its own control.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{boxed_int, build_registry, call, class_name_of, new_linked_hashmap, MockCtx};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ClassId, ObjectRef, Value};

const LHM: &str = "java/util/LinkedHashMap";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";

/// The real JDK slot order, shared with `LHM_NODE_*` in `native-collections`:
/// `hash@0, key@1, value@2, next@3, before@4, after@5`.
const HASH: usize = 0;
const KEY: usize = 1;
const VALUE: usize = 2;
const BEFORE: usize = 4;
const AFTER: usize = 5;

fn put(reg: &NativeMethodRegistry, ctx: &mut MockCtx, m: ObjectRef, k: Value, v: Value) {
    call(reg, ctx, LHM, "put", PUT, &[Value::Object(Some(m)), k, v]).unwrap();
}

fn int_of(ctx: &MockCtx, v: Value) -> Option<i32> {
    match v {
        Value::Object(Some(o)) => match ctx.get_field(o, 0) {
            Value::Int(n) => Some(n),
            _ => None,
        },
        Value::Int(n) => Some(n),
        _ => None,
    }
}

/// A `LinkedHashMap` whose runtime class is a SUBCLASS, so
/// `lhm_remove_eldest_hook_decision` arms the `removeEldestEntry` dispatch
/// (a plain `java/util/LinkedHashMap` never calls the hook — it has no
/// override to call).
fn new_lru_subclass(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let lhm = new_linked_hashmap(reg, ctx, false);
    let sub = ctx.ensure_class_initialized("test/LruCache").unwrap();
    ctx.set_object_class_id_for_test(lhm, sub);
    lhm
}

/// The single argument the hook handed to `removeEldestEntry`, or `None` if it
/// was never called.
fn eldest_arg(ctx: &MockCtx) -> Option<ObjectRef> {
    ctx.invoke_virtual_log()
        .into_iter()
        .find(|(_, m, _, _)| m == "removeEldestEntry")
        .and_then(|(_, _, _, a)| match a.first() {
            Some(Value::Object(Some(o))) => Some(*o),
            _ => None,
        })
}

/// The guard that would have caught the original defect on its own: the node
/// allocator must ask the class system for the REAL nested node class, and must
/// never ask for the invented one.
///
/// `MockCtx::ensure_class_initialized` registers whatever name it is handed, so
/// "was this name ever requested?" is exactly `class_id_by_name(..).is_some()`
/// after a put.
#[test]
fn node_allocator_asks_for_the_real_jdk_entry_class_and_never_the_invented_one() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);

    // Nothing has touched either name yet.
    assert!(
        ctx.class_id_by_name("java/util/LinkedHashMap$Entry")
            .is_none(),
        "the entry class must not be registered before the first put",
    );

    let k = boxed_int(&mut ctx, 1);
    let v = boxed_int(&mut ctx, 100);
    put(&reg, &mut ctx, lhm, k, v);

    assert!(
        ctx.class_id_by_name("java/util/LinkedHashMap$Entry")
            .is_some(),
        "lhm_alloc_node must allocate the real java/util/LinkedHashMap$Entry",
    );
    assert!(
        ctx.class_id_by_name("java/util/LinkedHashMap$Node")
            .is_none(),
        "java/util/LinkedHashMap$Node does not exist in the JDK — the node \
         allocator must never ask for it (this is the class name that made \
         LinkedHashMap.afterNodeRemoval's `(LinkedHashMap.Entry) e` throw)",
    );
}

/// The eldest-entry hook must hand the override the LIVE head node, which is
/// what HotSpot's `afterNodeInsertion` does (`removeEldestEntry(first)` where
/// `first = head`) — not a copied `SimpleImmutableEntry`, whose `setValue`
/// throws and which is never `==` the entry in the map.
#[test]
fn remove_eldest_entry_receives_the_live_head_node_not_a_copy() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_lru_subclass(&reg, &mut ctx);

    let k1 = boxed_int(&mut ctx, 1);
    let v1 = boxed_int(&mut ctx, 100);
    put(&reg, &mut ctx, lhm, k1, v1);

    ctx.clear_invoke_virtual_log();
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = boxed_int(&mut ctx, 200);
    put(&reg, &mut ctx, lhm, k2, v2);

    let eldest = eldest_arg(&ctx).expect("removeEldestEntry was never dispatched");
    assert_eq!(
        class_name_of(&ctx, eldest).as_deref(),
        Some("java/util/LinkedHashMap$Entry"),
        "the hook must pass the real node; a \
         java/util/AbstractMap$SimpleImmutableEntry here means the \
         copy-the-key-and-value workaround came back",
    );

    // It is the head (insertion order), i.e. the entry inserted FIRST.
    assert_eq!(
        int_of(&ctx, ctx.get_field(eldest, KEY)),
        Some(1),
        "the eldest must be the first-inserted entry",
    );
    assert_eq!(int_of(&ctx, ctx.get_field(eldest, VALUE)), Some(100));
}

/// The node the hook handed over is the real one in every respect the JDK's
/// inherited `HashMap$Node` bodies rely on: `hash` is an `int` in slot 0 (a
/// reference there would be coerced to null by the descriptor-aware write path
/// once the object is bound to the real class), and the insertion-order links
/// live in the two slots the real `LinkedHashMap$Entry` declares.
#[test]
fn the_node_handed_over_carries_the_real_jdk_slot_layout() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_lru_subclass(&reg, &mut ctx);

    for i in 1..=3 {
        let k = boxed_int(&mut ctx, i);
        let v = boxed_int(&mut ctx, i * 100);
        ctx.clear_invoke_virtual_log();
        put(&reg, &mut ctx, lhm, k, v);
    }

    let head = eldest_arg(&ctx).expect("removeEldestEntry was never dispatched");

    assert!(
        matches!(ctx.get_field(head, HASH), Value::Int(_)),
        "slot 0 is `final int hash` on the real HashMap$Node — anything but an \
         Int here is the legacy key@0 layout",
    );
    assert!(
        matches!(ctx.get_field(head, BEFORE), Value::Object(None)),
        "the head's `before` link must be null",
    );
    // 3 entries were inserted, so the head has a successor.
    let after = match ctx.get_field(head, AFTER) {
        Value::Object(Some(o)) => o,
        other => panic!("head.after should link to the second entry, got {other:?}"),
    };
    assert_eq!(
        class_name_of(&ctx, after).as_deref(),
        Some("java/util/LinkedHashMap$Entry"),
        "every node on the insertion-order chain is a real entry",
    );
    assert_eq!(int_of(&ctx, ctx.get_field(after, KEY)), Some(2));
}

/// Sanity: `set_object_class_id_for_test` really did make the receiver a
/// subclass. Without it the hook never fires and the two tests above would pass
/// vacuously on an `expect` that never runs — so assert the opposite direction
/// too.
#[test]
fn a_plain_linked_hash_map_never_dispatches_the_hook() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);

    for i in 1..=3 {
        let k = boxed_int(&mut ctx, i);
        let v = boxed_int(&mut ctx, i * 100);
        put(&reg, &mut ctx, lhm, k, v);
    }

    assert!(
        eldest_arg(&ctx).is_none(),
        "java/util/LinkedHashMap has no removeEldestEntry override to call",
    );
    let _: ClassId = ctx.class_id_by_name(LHM).expect("LHM registered");
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! Regression coverage for native roots held across Java callbacks.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{
    boxed_int, build_registry, call, new_arraylist, new_concurrent_hashmap, new_hashmap,
    new_linked_hashmap, new_treemap, MockCtx,
};
use cratonvm_native_api::NativeContext;
use cratonvm_types::Value;

const STREAM: &str = "java/util/stream/Stream";
const AL: &str = "java/util/ArrayList";
const HM: &str = "java/util/HashMap";
const CHM: &str = "java/util/concurrent/ConcurrentHashMap";
const LHM: &str = "java/util/LinkedHashMap";
const TM: &str = "java/util/TreeMap";
const AD: &str = "java/util/ArrayDeque";
// The class `ArrayDeque.iterator()` hands out since `e9b788959`. It is the one
// HotSpot 25.0.4+7 names; the fabrication `java/util/ArrayDeque$Itr` it replaced
// is declared by no JDK image, which is why `--jdk-only` refused it.
const AD_ITR: &str = "java/util/ArrayDeque$DeqIterator";
const COLLECTIONS: &str = "java/util/Collections";
const UNMOD_COLLECTION: &str = "cratonvm/internal/UnmodifiableCollection";
const UNMOD_LIST: &str = "cratonvm/internal/UnmodifiableList";
const UNMOD_LIST_ITR: &str = "cratonvm/internal/UnmodifiableListItr";

fn object_value(ctx: &mut MockCtx, class_id: u32) -> Value {
    Value::Object(Some(ctx.alloc_object_simple(class_id)))
}

#[test]
fn generic_snapshot_iterator_roots_array_and_shell_across_allocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let snapshot = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 2);
    let e1 = object_value(&mut ctx, 891);
    let e2 = object_value(&mut ctx, 892);
    ctx.set_array_element(snapshot, 0, e1);
    ctx.set_array_element(snapshot, 1, e2);

    ctx.set_relocate_pins_on_alloc(true);
    let iterator =
        match cratonvm_native_collections::make_iterator_from_array(&mut ctx, snapshot, 2).unwrap()
        {
            Some(Value::Object(Some(iterator))) => iterator,
            other => panic!("expected snapshot iterator, got {other:?}"),
        };
    ctx.set_relocate_pins_on_alloc(false);

    let forwarded_snapshot = match ctx.get_field(iterator, 0) {
        Value::Object(Some(snapshot)) => snapshot,
        other => panic!("iterator lost snapshot array: {other:?}"),
    };
    assert_ne!(
        forwarded_snapshot, snapshot,
        "snapshot must be read through its forwarding pin"
    );
    for expected_cid in [891, 892] {
        assert_eq!(
            call(
                &reg,
                &mut ctx,
                "java/util/HashMap$KeyItr",
                "hasNext",
                "()Z",
                &[Value::Object(Some(iterator))],
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        let value = call(
            &reg,
            &mut ctx,
            "java/util/HashMap$KeyItr",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(iterator))],
        )
        .unwrap();
        let object = match value {
            Some(Value::Object(Some(object))) => object,
            other => panic!("expected snapshot element, got {other:?}"),
        };
        assert_eq!(ctx.class_id_of_object(object).as_u32(), expected_cid);
    }
}

/// The `ArrayDeque` iterator's snapshot graph, rooted across three allocations.
///
/// # The shape this walks, and why it is walked rather than indexed
///
/// This test used to read the backing deque out of `iterator` field 2, because
/// `native_ad_iterator` minted the fabrication `java/util/ArrayDeque$Itr` with
/// `field 0 = snapshot array, 1 = cursor, 2 = backing deque`. On 2026-08-29
/// (`e9b788959`) that fabrication was retired: the site now mints the class
/// HotSpot hands out, `java/util/ArrayDeque$DeqIterator`, through
/// `alloc_family_snapshot_iterator`, and the graph gained a level:
///
/// ```text
///   DeqIterator[b + 0] -> ArrayList-shaped wrapper
///                            wrapper[elementData] -> Object[n + 1]
///                                                      [0..n) elements
///                                                      [n]    THE SOURCE DEQUE
///   DeqIterator[b + 1] = cursor  = 0
///   DeqIterator[b + 2] = lastRet = -1
/// ```
///
/// where `b = object_num_fields(itr) - 3`, the carrier's own declared fields.
/// The old assertion did not fail loudly on a changed contract — it read
/// `lastRet` and reported `iterator lost backing deque: Int(-1)`, which names a
/// coercion defect that is not there. So this walks the graph structurally
/// (trailing-three convention, the wrapper's one array-valued field, the
/// array's trailing capacity slot) instead of hard-coding three indices that
/// belong to three different classes. A layout change now shows up as a walk
/// that cannot find the next hop, with the hop named.
///
/// # What is under test is unchanged
///
/// Every allocation on that path can move the source deque, the elements and
/// the array, so each must be read back through its pin before it is stored.
/// The assertions that matter are the `assert_ne!`s: a pointer equal to the
/// pre-allocation one is a value that was NOT read back through its forwarding
/// pin, which is the defect this file exists for.
/// # IGNORED: this test outlived the thing it tests
///
/// `java/util/ArrayDeque.iterator()` has had no native registration since
/// 2026-08-30. That was deliberate and is argued at the registration site: the
/// JDK's `DeqIterator` is fail-fast off a PHYSICAL ring-buffer index, no
/// snapshot reproduces it, so the mint was retired and the real bytecode left
/// to run. ArrayDeque left `VALUES_ITR_CARRIERS` in the same change.
///
/// The removal did not update this test. It has failed on every run of this
/// file since, with `native not registered:
/// java/util/ArrayDeque.iterator()Ljava/util/Iterator;` — a RED GATE that says
/// nothing, because the registration it asks for is one the tree decided not to
/// have. Found 2026-09-02, red on a clean `dev` checkout.
///
/// Ignored rather than deleted: this crate's own note on the dormant
/// `ArrayDeque$Itr` rows says "deleting registrations is the shadow-retirement
/// lane's edit and wants its own census", and the same boundary applies to the
/// test that was paired with them. Its assertions are still the right ones IF
/// an ArrayDeque iterator is ever minted again — including the one that caught
/// a first attempt at exactly that on 2026-09-02, by requiring
/// `ArrayDeque$DeqIterator` (what HotSpot hands out) where a snapshot minting
/// `ArrayDeque$Itr` would have armed the dormant rows.
#[ignore = "ArrayDeque.iterator() is deliberately unregistered since 2026-08-30;             this test asks for a registration the tree decided not to have.             Un-ignore if ArrayDeque rejoins VALUES_ITR_CARRIERS."]
#[test]
fn array_deque_iterator_roots_snapshot_graph_across_allocations() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    // `DeqIterator` must report its REAL declared width, or the carrier path is
    // not the one under test. `java.util.ArrayDeque$DeqIterator` declares
    // `cursor`, `remaining` and `lastRet`, plus javac's synthetic `this$0` for
    // the inner class: four. `alloc_arraylist_iterator_as` mints
    // `class_num_total_fields + 3`, and `al_itr_alt_base` recognises its own
    // mint by that exact width -- behind an early-out for anything four fields
    // or narrower. With the mock's default of zero the iterator would be minted
    // three wide, take that early-out, and be delegated to bytecode the mock
    // does not have, so `hasNext` would answer `Ok(None)` and the iteration half
    // of this test would be measuring nothing.
    ctx.declare_class_fields(AD_ITR, 4);
    let deque_cid = ctx.ensure_class_initialized(AD).unwrap();
    let deque = ctx.alloc_object(deque_cid, 4);
    call(
        &reg,
        &mut ctx,
        AD,
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();
    let e1 = object_value(&mut ctx, 901);
    let e2 = object_value(&mut ctx, 902);
    for value in [e1, e2] {
        call(
            &reg,
            &mut ctx,
            AD,
            "addLast",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(deque)), value],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_alloc(true);
    let iterator = match call(
        &reg,
        &mut ctx,
        AD,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    {
        Some(Value::Object(Some(iterator))) => iterator,
        other => panic!("expected ArrayDeque iterator, got {other:?}"),
    };
    ctx.set_relocate_pins_on_alloc(false);

    // The class is part of the contract, not decoration: the whole point of
    // `e9b788959` was that this site stopped answering a name no JDK image
    // declares. A silent return to a fabrication would otherwise leave every
    // assertion below still passing.
    assert_eq!(
        ctx.class_name_of_id(ctx.class_id_of_object(iterator))
            .as_deref(),
        Some(AD_ITR),
        "ArrayDeque.iterator() must hand out the class HotSpot hands out"
    );

    // Hop 1: the wrapper, in the first of the three trailing snapshot slots.
    let carrier_fields = ctx.object_num_fields(iterator);
    assert_eq!(
        carrier_fields, 7,
        "the mint is `class_num_total_fields + 3`: DeqIterator's four plus the triple"
    );
    let wrapper = match ctx.get_field(iterator, carrier_fields - 3) {
        Value::Object(Some(wrapper)) => wrapper,
        other => panic!("iterator lost its snapshot wrapper: {other:?}"),
    };

    // Hop 2: the wrapper's backing array. Found by kind rather than by slot —
    // `al_slots` resolves `elementData` by name and the mock need not agree
    // with any particular index.
    let arr = (0..ctx.object_num_fields(wrapper))
        .find_map(|i| match ctx.get_field(wrapper, i) {
            Value::Object(Some(o))
                if ctx.heap_kind_of(o) == cratonvm_types::ObjectKind::Array =>
            {
                Some(o)
            }
            _ => None,
        })
        .expect("snapshot wrapper lost its backing array");
    assert_eq!(
        ctx.array_length(arr),
        3,
        "snapshot array is the two elements plus the trailing source slot"
    );

    // Hop 3: the source deque, in the array's trailing capacity slot. This is
    // the ref `propagate_list_removal` reads to route `Iterator.remove()` back
    // to `native_ad_remove_first_occurrence`, so losing it is a silently
    // non-mutating `remove()`, not a crash.
    let backing = match ctx.get_array_element(arr, 2) {
        Value::Object(Some(backing)) => backing,
        other => panic!("snapshot array lost the source deque: {other:?}"),
    };
    assert_ne!(
        backing, deque,
        "backing deque must be read back through its forwarding pin"
    );
    assert_eq!(ctx.class_id_of_object(backing), deque_cid);

    // The elements travel the same path and get the same treatment.
    for (i, (before, expected_cid)) in [(e1, 901u32), (e2, 902u32)].into_iter().enumerate() {
        let stored = match ctx.get_array_element(arr, i) {
            Value::Object(Some(stored)) => stored,
            other => panic!("snapshot array lost element {i}: {other:?}"),
        };
        assert_eq!(ctx.class_id_of_object(stored).as_u32(), expected_cid);
        assert_ne!(
            Value::Object(Some(stored)),
            before,
            "element {i} must be read back through its forwarding pin"
        );
    }

    // And the iterator walks them, through the natives the
    // `VALUES_ITR_CARRIERS` loop bound to the real carrier class.
    for expected_cid in [901, 902] {
        assert_eq!(
            call(
                &reg,
                &mut ctx,
                AD_ITR,
                "hasNext",
                "()Z",
                &[Value::Object(Some(iterator))],
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        let value = call(
            &reg,
            &mut ctx,
            AD_ITR,
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(iterator))],
        )
        .unwrap();
        let object = match value {
            Some(Value::Object(Some(object))) => object,
            other => panic!("expected snapshot element, got {other:?}"),
        };
        assert_eq!(ctx.class_id_of_object(object).as_u32(), expected_cid);
    }
    // The trailing source slot is capacity, not content: `size` is 2, so the
    // walk must stop before it rather than hand the deque out as an element.
    assert_eq!(
        call(
            &reg,
            &mut ctx,
            AD_ITR,
            "hasNext",
            "()Z",
            &[Value::Object(Some(iterator))],
        )
        .unwrap(),
        Some(Value::Int(0))
    );
}

#[test]
fn arraylist_iterator_roots_backing_list_across_allocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = new_arraylist(&reg, &mut ctx);
    let list_cid = ctx.class_id_of_object(list);
    let e1 = object_value(&mut ctx, 911);
    let e2 = object_value(&mut ctx, 912);
    for value in [e1, e2] {
        call(
            &reg,
            &mut ctx,
            AL,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), value],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_alloc(true);
    let iterator = match call(
        &reg,
        &mut ctx,
        AL,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(list))],
    )
    .unwrap()
    {
        Some(Value::Object(Some(iterator))) => iterator,
        other => panic!("expected ArrayList iterator, got {other:?}"),
    };
    ctx.set_relocate_pins_on_alloc(false);

    let backing = match ctx.get_field(iterator, 3) {
        Value::Object(Some(backing)) => backing,
        other => panic!("iterator lost backing list: {other:?}"),
    };
    assert_ne!(
        backing, list,
        "backing list must be read back through its forwarding pin"
    );
    assert_eq!(ctx.class_id_of_object(backing), list_cid);

    for expected_cid in [911, 912] {
        assert_eq!(
            call(
                &reg,
                &mut ctx,
                "java/util/ArrayList$Itr",
                "hasNext",
                "()Z",
                &[Value::Object(Some(iterator))],
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        let value = call(
            &reg,
            &mut ctx,
            "java/util/ArrayList$Itr",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(iterator))],
        )
        .unwrap();
        let object = match value {
            Some(Value::Object(Some(object))) => object,
            other => panic!("expected list element, got {other:?}"),
        };
        assert_eq!(ctx.class_id_of_object(object).as_u32(), expected_cid);
    }
    assert_eq!(
        call(
            &reg,
            &mut ctx,
            "java/util/ArrayList$Itr",
            "hasNext",
            "()Z",
            &[Value::Object(Some(iterator))],
        )
        .unwrap(),
        Some(Value::Int(0))
    );
}

#[test]
fn unmodifiable_wrapper_roots_backing_and_wrapper_across_allocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let backing = new_arraylist(&reg, &mut ctx);
    let backing_cid = ctx.class_id_of_object(backing);

    ctx.set_relocate_pins_on_alloc(true);
    let wrapper = match call(
        &reg,
        &mut ctx,
        COLLECTIONS,
        "unmodifiableCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        &[Value::Object(Some(backing))],
    )
    .unwrap()
    {
        Some(Value::Object(Some(wrapper))) => wrapper,
        other => panic!("expected unmodifiable wrapper, got {other:?}"),
    };
    ctx.set_relocate_pins_on_alloc(false);

    assert_eq!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(wrapper))
            .as_deref(),
        Some(UNMOD_COLLECTION)
    );
    let forwarded_backing = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(backing)) => backing,
        other => panic!("wrapper lost backing collection: {other:?}"),
    };
    assert_ne!(
        forwarded_backing, backing,
        "backing collection must be read through its forwarding pin"
    );
    assert_eq!(ctx.class_id_of_object(forwarded_backing), backing_cid);
}

#[test]
fn unmodifiable_list_iterator_roots_snapshot_graph_across_allocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let backing = new_arraylist(&reg, &mut ctx);
    let wrapper = match call(
        &reg,
        &mut ctx,
        COLLECTIONS,
        "unmodifiableList",
        "(Ljava/util/List;)Ljava/util/List;",
        &[Value::Object(Some(backing))],
    )
    .unwrap()
    {
        Some(Value::Object(Some(wrapper))) => wrapper,
        other => panic!("expected unmodifiable list, got {other:?}"),
    };
    let snapshot = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 2);
    let e1 = object_value(&mut ctx, 921);
    let e2 = object_value(&mut ctx, 922);
    ctx.set_array_element(snapshot, 0, e1);
    ctx.set_array_element(snapshot, 1, e2);
    ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(snapshot)))));

    ctx.set_relocate_pins_on_alloc(true);
    let iterator = match call(
        &reg,
        &mut ctx,
        UNMOD_LIST,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(wrapper))],
    )
    .unwrap()
    {
        Some(Value::Object(Some(iterator))) => iterator,
        other => panic!("expected unmodifiable list iterator, got {other:?}"),
    };
    ctx.set_relocate_pins_on_alloc(false);

    let forwarded_snapshot = match ctx.get_field(iterator, 0) {
        Value::Object(Some(snapshot)) => snapshot,
        other => panic!("iterator lost snapshot array: {other:?}"),
    };
    assert_ne!(
        forwarded_snapshot, snapshot,
        "snapshot must be read through its forwarding pin"
    );
    assert_eq!(ctx.array_length(forwarded_snapshot), 2);
    for expected_cid in [921, 922] {
        assert_eq!(
            call(
                &reg,
                &mut ctx,
                UNMOD_LIST_ITR,
                "hasNext",
                "()Z",
                &[Value::Object(Some(iterator))],
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        let value = call(
            &reg,
            &mut ctx,
            UNMOD_LIST_ITR,
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(iterator))],
        )
        .unwrap();
        let object = match value {
            Some(Value::Object(Some(object))) => object,
            other => panic!("expected snapshot element, got {other:?}"),
        };
        assert_eq!(ctx.class_id_of_object(object).as_u32(), expected_cid);
    }
    assert_eq!(
        call(
            &reg,
            &mut ctx,
            UNMOD_LIST_ITR,
            "hasNext",
            "()Z",
            &[Value::Object(Some(iterator))],
        )
        .unwrap(),
        Some(Value::Int(0))
    );
}

#[test]
fn stream_for_each_reads_forwarded_consumer_and_elements() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();

    let e1 = object_value(&mut ctx, 101);
    let e2 = object_value(&mut ctx, 102);
    let consumer = ctx.alloc_object_simple(200);
    let stream = match cratonvm_native_collections::make_stream_from_elements(&mut ctx, &[e1, e2])
        .unwrap()
    {
        Some(Value::Object(Some(s))) => s,
        other => panic!("expected stream object, got {other:?}"),
    };

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        STREAM,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        &[Value::Object(Some(stream)), Value::Object(Some(consumer))],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(log.len(), 2, "forEach should invoke the consumer twice");
    assert_eq!(log[0].0, consumer.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        consumer.as_ptr() as usize,
        "consumer must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[0], e2,
        "second element must be re-read from its native pin after callback GC"
    );
}

#[test]
fn arraylist_remove_if_reads_forwarded_predicate_and_elements() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let al = new_arraylist(&reg, &mut ctx);
    let e1 = object_value(&mut ctx, 301);
    let e2 = object_value(&mut ctx, 302);
    let predicate = ctx.alloc_object_simple(400);

    call(
        &reg,
        &mut ctx,
        AL,
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), e1],
    )
    .unwrap();
    call(
        &reg,
        &mut ctx,
        AL,
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), e2],
    )
    .unwrap();

    ctx.set_invoke_virtual_result(Ok(Some(Value::Int(1))));
    ctx.set_relocate_pins_on_invoke(true);
    let removed = call(
        &reg,
        &mut ctx,
        AL,
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
        &[Value::Object(Some(al)), Value::Object(Some(predicate))],
    )
    .unwrap();

    assert_eq!(removed, Some(Value::Int(1)));
    let log = ctx.invoke_virtual_log();
    assert_eq!(log.len(), 2, "removeIf should test both elements");
    assert_eq!(log[0].0, predicate.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        predicate.as_ptr() as usize,
        "predicate must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[0], e2,
        "second element must be re-read from its native pin after callback GC"
    );
}

#[test]
fn linked_hashmap_for_each_reads_forwarded_consumer() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 501);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 502);
    let consumer = ctx.alloc_object_simple(503);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            LHM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(lhm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        LHM,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        &[Value::Object(Some(lhm)), Value::Object(Some(consumer))],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(
        log.len(),
        2,
        "LinkedHashMap.forEach should visit two entries"
    );
    assert_eq!(log[0].0, consumer.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        consumer.as_ptr() as usize,
        "BiConsumer must be re-read from its native pin after callback GC"
    );
}

#[test]
fn treemap_for_each_reads_forwarded_action_and_pairs() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_treemap(&reg, &mut ctx);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 601);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 602);
    let action = ctx.alloc_object_simple(603);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            TM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(tm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        TM,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        &[Value::Object(Some(tm)), Value::Object(Some(action))],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(log.len(), 2, "TreeMap.forEach should visit two entries");
    assert_eq!(log[0].0, action.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        action.as_ptr() as usize,
        "BiConsumer must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[0], k2,
        "second key must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[1], v2,
        "second value must be re-read from its native pin after callback GC"
    );
}

#[test]
fn hashmap_replace_all_reads_forwarded_function_and_entries() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 701);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 702);
    let function = ctx.alloc_object_simple(703);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            HM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(hm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        HM,
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
        &[Value::Object(Some(hm)), Value::Object(Some(function))],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(log.len(), 2, "HashMap.replaceAll should visit two entries");
    assert_eq!(log[0].0, function.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        function.as_ptr() as usize,
        "BiFunction must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[0], k2,
        "second key must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[1], v2,
        "second value must be re-read from its native pin after callback GC"
    );
}

#[test]
fn concurrent_hashmap_for_each_key_reads_forwarded_action_and_keys() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let chm = new_concurrent_hashmap(&reg, &mut ctx);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 801);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 802);
    let action = ctx.alloc_object_simple(803);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            CHM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(chm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        CHM,
        "forEachKey",
        "(JLjava/util/function/Consumer;)V",
        &[
            Value::Object(Some(chm)),
            Value::Long(1),
            Value::Object(Some(action)),
        ],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(
        log.len(),
        2,
        "ConcurrentHashMap.forEachKey should visit two keys"
    );
    assert_eq!(log[0].0, action.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        action.as_ptr() as usize,
        "Consumer must be re-read from its native pin after callback GC"
    );
    assert!(
        log[1].3[0] != k1 && log[1].3[0] != k2,
        "second key must be re-read from its native pin after callback GC"
    );
}

#[test]
fn concurrent_hashmap_search_reads_forwarded_function_and_entries() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let chm = new_concurrent_hashmap(&reg, &mut ctx);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 901);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 902);
    let function = ctx.alloc_object_simple(903);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            CHM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(chm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    let result = call(
        &reg,
        &mut ctx,
        CHM,
        "search",
        "(JLjava/util/function/BiFunction;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Long(1),
            Value::Object(Some(function)),
        ],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Object(None)));
    let log = ctx.invoke_virtual_log();
    assert_eq!(
        log.len(),
        2,
        "ConcurrentHashMap.search should visit two entries"
    );
    assert_eq!(log[0].0, function.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        function.as_ptr() as usize,
        "BiFunction must be re-read from its native pin after callback GC"
    );
    assert!(
        log[1].3[0] != k1 && log[1].3[0] != k2,
        "second key must be re-read from its native pin after callback GC"
    );
    assert!(
        log[1].3[1] != v1 && log[1].3[1] != v2,
        "second value must be re-read from its native pin after callback GC"
    );
}

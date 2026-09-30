// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! gc-common w20-a: natives that held an argument or an intermediate object in
//! a bare Rust local across a GC point.
//!
//! The production native-call funnel roots every argument slot and the
//! collector remaps THAT root, but the native's own copy of the argument is
//! never rewritten. Each test below reproduces that: the test roots the
//! arguments itself (as the funnel does), turns on the mock's moving
//! collection (`relocate_pins_on_alloc` / `relocate_pins_on_invoke`, which
//! move every rooted object and invalidate its old address), and checks an
//! outcome the pre-fix code got wrong because it went on using the copy.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{boxed_int, build_registry, call, MockCtx};
use cratonvm_native_api::NativeContext;
use cratonvm_types::{ClassId, ObjectRef, Value};

const SJ: &str = "java/util/StringJoiner";
const UNMOD_ENTRY_SET: &str = "cratonvm/internal/UnmodifiableEntrySet";

fn obj(v: Option<Value>) -> ObjectRef {
    match v {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an object, got {other:?}"),
    }
}

/// `StringJoiner(delim)` / `add` / `toString` on the legacy synthetic layout
/// (the mock resolves none of the real `StringJoiner` fields), with the
/// receiver and argument rooted by the "funnel" and every allocation moving
/// them. Before w20-a the constructor stored its element list, delimiter and
/// markers through the receiver's pre-allocation address -- the stores were
/// dropped, every `add` found no element list and returned early, and the
/// joiner rendered as the empty string.
#[test]
fn w20a_string_joiner_survives_a_moving_collection_at_every_allocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let cid = ctx.ensure_class_initialized(SJ).unwrap();
    let sj = ctx.alloc_object(cid, 5);
    let delim = ctx.create_string(",");

    // `<init>(delim)`, receiver and argument rooted like the funnel roots them.
    let base = ctx.pin_native_root(sj);
    ctx.pin_native_root(delim);
    ctx.set_relocate_pins_on_alloc(true);
    call(
        &reg,
        &mut ctx,
        SJ,
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        &[Value::Object(Some(sj)), Value::Object(Some(delim))],
    )
    .unwrap();
    ctx.set_relocate_pins_on_alloc(false);
    let mut sj = ctx.read_native_pin(base, sj);
    ctx.unpin_native_roots(base);
    assert_eq!(ctx.pin_depth(), 0, "the constructor must release its pins");

    // Twelve adds: past the legacy list's initial capacity of ten, so the
    // growth branch (which also stored the element through a stale copy) runs.
    for i in 0..12 {
        let s = ctx.create_string(&i.to_string());
        let base = ctx.pin_native_root(sj);
        ctx.pin_native_root(s);
        ctx.set_relocate_pins_on_alloc(true);
        let ret = call(
            &reg,
            &mut ctx,
            SJ,
            "add",
            "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
            &[Value::Object(Some(sj)), Value::Object(Some(s))],
        )
        .unwrap();
        ctx.set_relocate_pins_on_alloc(false);
        sj = ctx.read_native_pin(base, sj);
        ctx.unpin_native_roots(base);
        assert_eq!(ctx.pin_depth(), 0, "add #{i} must release its pins");
        assert_eq!(
            obj(ret),
            sj,
            "add #{i} must answer the receiver's current address"
        );
    }

    let text = call(
        &reg,
        &mut ctx,
        SJ,
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sj))],
    )
    .unwrap();
    let text = ctx.read_string(obj(text)).unwrap();
    assert_eq!(text, "0,1,2,3,4,5,6,7,8,9,10,11");
}

/// `Map.ofEntries(e1, e2)` with the entry array rooted by the "funnel" and a
/// moving collection after every Java call. Before w20-a the array was indexed
/// through its entry address after `e1.getKey()` ran, so `e2` read back as
/// null and was silently skipped: one `getKey` instead of two.
#[test]
fn w20a_map_of_entries_reads_every_entry_after_a_moving_collection() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let arr = ctx.new_ref_array(ClassId::new(0), 2);
    let e1 = ctx.alloc_object_simple(2001);
    let e2 = ctx.alloc_object_simple(2002);
    ctx.set_array_element(arr, 0, Value::Object(Some(e1)));
    ctx.set_array_element(arr, 1, Value::Object(Some(e2)));
    let (k1, v1, k2, v2) = (
        boxed_int(&mut ctx, 1),
        boxed_int(&mut ctx, 10),
        boxed_int(&mut ctx, 2),
        boxed_int(&mut ctx, 20),
    );
    ctx.set_invoke_virtual_results(vec![Ok(Some(k1)), Ok(Some(v1)), Ok(Some(k2)), Ok(Some(v2))]);

    let base = ctx.pin_native_root(arr);
    ctx.set_relocate_pins_on_invoke(true);
    // The map build afterwards runs more (mocked) Java; its answer is not what
    // this test is about, only that both entries were read.
    let _ = call(
        &reg,
        &mut ctx,
        "java/util/Map",
        "ofEntries",
        "([Ljava/util/Map$Entry;)Ljava/util/Map;",
        &[Value::Object(Some(arr))],
    );
    ctx.set_relocate_pins_on_invoke(false);
    ctx.unpin_native_roots(base);
    assert_eq!(ctx.pin_depth(), 0, "ofEntries must release its pins");

    let log = ctx.invoke_virtual_log();
    let get_keys = log.iter().filter(|(_, m, _, _)| m == "getKey").count();
    let get_values = log.iter().filter(|(_, m, _, _)| m == "getValue").count();
    assert_eq!(
        get_keys, 2,
        "both entries must be asked for their key: {log:?}"
    );
    assert_eq!(
        get_values, 2,
        "both entries must be asked for their value: {log:?}"
    );
}

/// `UnmodifiableEntrySet.forEach(consumer)` with a moving collection after
/// every Java call. Before w20-a `next()` was dispatched on the iterator
/// address read before `hasNext()` ran, and `accept` on the consumer's entry
/// address.
#[test]
fn w20a_unmodifiable_entry_set_for_each_rereads_iterator_and_consumer() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let set_cid = ctx.ensure_class_initialized(UNMOD_ENTRY_SET).unwrap();
    let set = ctx.alloc_object(set_cid, 2);
    let backing = ctx.alloc_object_simple(2101);
    ctx.set_field(set, 0, Value::Object(Some(backing)));
    let consumer = ctx.alloc_object_simple(2102);
    let itr = ctx.alloc_object_simple(2103);
    let entry = ctx.alloc_object_simple(2104);
    ctx.set_invoke_virtual_results(vec![
        Ok(Some(Value::Object(Some(itr)))),   // backing.iterator()
        Ok(Some(Value::Int(1))),              // hasNext()
        Ok(Some(Value::Object(Some(entry)))), // next()
        Ok(None),                             // consumer.accept(wrapped)
        Ok(Some(Value::Int(0))),              // hasNext()
    ]);

    let base = ctx.pin_native_root(set);
    ctx.pin_native_root(consumer);
    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        UNMOD_ENTRY_SET,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        &[Value::Object(Some(set)), Value::Object(Some(consumer))],
    )
    .unwrap();
    ctx.set_relocate_pins_on_invoke(false);
    ctx.unpin_native_roots(base);
    assert_eq!(ctx.pin_depth(), 0, "forEach must release its pins");

    let log = ctx.invoke_virtual_log();
    let at = |m: &str| {
        log.iter()
            .position(|(_, name, _, _)| name == m)
            .unwrap_or_else(|| panic!("no `{m}` call in {log:?}"))
    };
    let (has_next, next, accept) = (at("hasNext"), at("next"), at("accept"));
    assert_ne!(
        log[next].0, log[has_next].0,
        "`next` must be dispatched on the iterator's post-`hasNext` address"
    );
    assert_ne!(
        log[accept].0,
        consumer.as_ptr() as usize,
        "`accept` must be dispatched on the consumer's current address, not its entry copy"
    );
}

/// `CopyOnWriteArrayList.addAll(c)` on the synthetic two-field layout (the
/// mock resolves no real `array`/`lock` field), with a moving collection after
/// every Java call. Before w20-a the receiver was pinned only AFTER
/// `c.toArray()` ran, so the pin held the receiver's pre-`toArray` address and
/// every `add` was dispatched on it.
#[test]
fn w20a_cowal_add_all_pins_the_receiver_before_to_array() {
    const COWAL: &str = "java/util/concurrent/CopyOnWriteArrayList";
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let cid = ctx.ensure_class_initialized(COWAL).unwrap();
    let list = ctx.alloc_object(cid, 2);
    let coll = ctx.alloc_object_simple(2201);
    let arr = ctx.new_ref_array(ClassId::new(0), 2);
    let (e1, e2) = (boxed_int(&mut ctx, 1), boxed_int(&mut ctx, 2));
    ctx.set_array_element(arr, 0, e1);
    ctx.set_array_element(arr, 1, e2);
    ctx.set_invoke_virtual_results(vec![
        Ok(Some(Value::Object(Some(arr)))), // coll.toArray()
        Ok(Some(Value::Int(1))),            // this.add(e1)
        Ok(Some(Value::Int(1))),            // this.add(e2)
    ]);

    let base = ctx.pin_native_root(list);
    ctx.pin_native_root(coll);
    ctx.set_relocate_pins_on_invoke(true);
    let ret = call(
        &reg,
        &mut ctx,
        COWAL,
        "addAll",
        "(Ljava/util/Collection;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(coll))],
    )
    .unwrap();
    ctx.set_relocate_pins_on_invoke(false);
    ctx.unpin_native_roots(base);
    assert_eq!(ctx.pin_depth(), 0, "addAll must release its pins");
    assert_eq!(ret, Some(Value::Int(1)));

    let log = ctx.invoke_virtual_log();
    let adds: Vec<usize> = log
        .iter()
        .filter(|(_, m, _, _)| m == "add")
        .map(|(recv, _, _, _)| *recv)
        .collect();
    assert_eq!(adds.len(), 2, "one `add` per element: {log:?}");
    for recv in &adds {
        assert_ne!(
            *recv,
            list.as_ptr() as usize,
            "`add` was dispatched on the receiver's pre-`toArray` address: {log:?}"
        );
    }
    assert_ne!(
        adds[0], adds[1],
        "the second `add` must use the address the first one's collection moved it to"
    );
}

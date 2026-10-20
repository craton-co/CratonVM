// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! The `java/util/List.add(Object)` interface native on a receiver that is not
//! ArrayList-layout (interpreter round i3 wave 79, lane L1;
//! `docs/internal/fixed-bugs/interpreter-i74-L1-list-add-interface-native-writes-arraylist-slots-into-a-foreign-receiver-FIXED-20261111.md`).
//!
//! The mock resolves ArrayList's real-JDK slots (`elementData` = 1, `size` =
//! 2), so a two-field `CopyOnWriteArrayList` (`lock`, `array`) passes the
//! `data_slot` count test `al_set_data` used to apply alone. Before the fix the
//! native stored a fresh one-element `Object[]` into field 1 -- the list's
//! `array` -- losing every earlier element. The mock answers no `Collection`
//! ancestry, so the delegation to the receiver's own `add` declines here and
//! the native's own path runs: it must leave both fields alone.

mod common;

use common::{build_registry, call, new_arraylist, MockCtx};
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::{ArrayElementType, Value};

const LIST: &str = "java/util/List";
const ADD: &str = "(Ljava/lang/Object;)Z";

#[test]
fn list_add_leaves_a_foreign_receivers_fields_alone() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    // The layout guard has teeth only once `java/util/ArrayList` is known.
    ctx.ensure_class_initialized("java/util/ArrayList").unwrap();
    let cid = ctx
        .ensure_class_initialized("java/util/concurrent/CopyOnWriteArrayList")
        .unwrap();
    let cowal = ctx.alloc_object(cid, 2);
    let lock_cid = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    let lock = ctx.alloc_object(lock_cid, 0);
    let array = ctx.new_array(ArrayElementType::Reference, 2);
    let e0 = ctx.alloc_object(lock_cid, 0);
    let e1 = ctx.alloc_object(lock_cid, 0);
    ctx.set_array_element(array, 0, Value::Object(Some(e0)));
    ctx.set_array_element(array, 1, Value::Object(Some(e1)));
    ctx.set_field(cowal, 0, Value::Object(Some(lock)));
    ctx.set_field(cowal, 1, Value::Object(Some(array)));

    let elem = ctx.alloc_object(lock_cid, 0);
    let _ = call(
        &reg,
        &mut ctx,
        LIST,
        "add",
        ADD,
        &[Value::Object(Some(cowal)), Value::Object(Some(elem))],
    );

    assert_eq!(
        ctx.get_field(cowal, 0),
        Value::Object(Some(lock)),
        "the receiver's field 0 (`lock`) must not be written"
    );
    assert_eq!(
        ctx.get_field(cowal, 1),
        Value::Object(Some(array)),
        "the receiver's field 1 (`array`) must not be replaced by an ArrayList buffer"
    );
    assert_eq!(ctx.array_length(array), 2);
    assert_eq!(ctx.get_array_element(array, 0), Value::Object(Some(e0)));
    assert_eq!(ctx.get_array_element(array, 1), Value::Object(Some(e1)));
}

#[test]
fn list_add_still_appends_to_an_arraylist() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let al = new_arraylist(&reg, &mut ctx);
    let obj_cid = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    for _ in 0..3 {
        let e = ctx.alloc_object(obj_cid, 0);
        let r = call(
            &reg,
            &mut ctx,
            LIST,
            "add",
            ADD,
            &[Value::Object(Some(al)), Value::Object(Some(e))],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(1)));
    }
    let size = call(
        &reg,
        &mut ctx,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(3)));
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! `Collections.unmodifiableList(x).get(i)` over a backing whose layout the
//! ArrayList natives cannot read.
//!
//! `native_unmod_get` bounds-checks before delegating, so that a wrapper over
//! an out-of-range index keeps raising `ArrayIndexOutOfBoundsException` rather
//! than the plain `IndexOutOfBoundsException` the backing `ArrayList` would
//! raise. The size for that check came from `al_state`, which returns its
//! `(None, 0)` **"this is not a layout I can read"** sentinel for every backing
//! that is not ArrayList/Vector-shaped — a real-JDK `java/util/LinkedList`, a
//! `cratonvm/internal/ArrayListSubList`, any foreign `AbstractSequentialList`.
//! Reading that sentinel as a size of 0 made `get` throw for **every** index on
//! a list whose `size()`, `iterator()`, `toString()` and `indexOf()` all
//! answered correctly.
//!
//! Live capture: spring-framework
//! `BeanRegistrationsAotContributionTests#applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles`,
//! where `SourceFile.getClassName` runs the generated source through QDox and
//! `DefaultJavaSource.getClasses()` hands back
//! `Collections.unmodifiableList(<LinkedList>)`. `size() == 1` passed the
//! assertion on the line above; `get(0)` on the line below threw.

mod common;

use common::{build_registry, call, new_arraylist, MockCtx};
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::Value;

const COLLECTIONS: &str = "java/util/Collections";
const UNMOD_LIST: &str = "cratonvm/internal/UnmodifiableList";
const GET: &str = "(I)Ljava/lang/Object;";

/// Wrap `backing` with the real `Collections.unmodifiableList` native.
fn wrap(
    reg: &cratonvm_native_api::NativeMethodRegistry,
    ctx: &mut MockCtx,
    backing: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    match call(
        reg,
        ctx,
        COLLECTIONS,
        "unmodifiableList",
        "(Ljava/util/List;)Ljava/util/List;",
        &[Value::Object(Some(backing))],
    )
    .unwrap()
    {
        Some(Value::Object(Some(w))) => w,
        other => panic!("expected an unmodifiable list, got {other:?}"),
    }
}

#[test]
fn get_over_a_linkedlist_backing_delegates_instead_of_throwing() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();

    // A `java/util/LinkedList` receiver: named (so the layout guard declines
    // it, as it does for the real JDK class) and carrying none of ArrayList's
    // `elementData`/`size` slots.
    // `al_is_arraylist_layout` stays lenient while `java/util/ArrayList` is
    // unknown to the context, so the layout guard only has teeth once that
    // class exists — as it always does in a real VM.
    ctx.ensure_class_initialized("java/util/ArrayList").unwrap();
    let cid = ctx
        .ensure_class_initialized("java/util/LinkedList")
        .unwrap();
    let backing = ctx.alloc_object(cid, 3);
    let wrapper = wrap(&reg, &mut ctx, backing);

    let element = Value::Object(Some(ctx.alloc_object_simple(7001)));
    ctx.set_invoke_virtual_result(Ok(Some(element)));
    ctx.clear_invoke_virtual_log();

    let got = call(
        &reg,
        &mut ctx,
        UNMOD_LIST,
        "get",
        GET,
        &[Value::Object(Some(wrapper)), Value::Int(0)],
    )
    .expect("get(0) over a LinkedList backing must not raise");
    assert_eq!(got, Some(element), "the delegate's element must come back");

    let log = ctx.invoke_virtual_log();
    assert!(
        log.iter().any(
            |(recv, name, desc, args)| *recv == backing.as_ptr() as usize
                && name == "get"
                && desc == GET
                && args == &[Value::Int(0)]
        ),
        "get must be delegated to the backing's own get(int); log = {log:?}",
    );
}

#[test]
fn get_over_an_arraylist_backing_still_bounds_checks() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();

    // The ArrayList backing IS readable, so the pre-check stays in force and
    // keeps raising ArrayIndexOutOfBoundsException — the behaviour measured
    // against HotSpot for `List.of()` and `List.of(a,b,c,d)`.
    let backing = new_arraylist(&reg, &mut ctx);
    let wrapper = wrap(&reg, &mut ctx, backing);

    let err = call(
        &reg,
        &mut ctx,
        UNMOD_LIST,
        "get",
        GET,
        &[Value::Object(Some(wrapper)), Value::Int(0)],
    )
    .expect_err("get(0) on an empty ArrayList backing must raise");
    assert!(
        format!("{err:?}").contains("ArrayIndexOutOfBounds"),
        "expected ArrayIndexOutOfBoundsException, got {err:?}",
    );
}

#[test]
fn a_negative_index_still_raises_on_a_foreign_backing() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();

    // `al_is_arraylist_layout` stays lenient while `java/util/ArrayList` is
    // unknown to the context, so the layout guard only has teeth once that
    // class exists — as it always does in a real VM.
    ctx.ensure_class_initialized("java/util/ArrayList").unwrap();
    let cid = ctx
        .ensure_class_initialized("java/util/LinkedList")
        .unwrap();
    let backing = ctx.alloc_object(cid, 3);
    let wrapper = wrap(&reg, &mut ctx, backing);

    let err = call(
        &reg,
        &mut ctx,
        UNMOD_LIST,
        "get",
        GET,
        &[Value::Object(Some(wrapper)), Value::Int(-1)],
    )
    .expect_err("get(-1) must raise regardless of the backing layout");
    assert!(
        format!("{err:?}").contains("ArrayIndexOutOfBounds"),
        "expected ArrayIndexOutOfBoundsException, got {err:?}",
    );
}

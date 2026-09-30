// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! `Collections.unmodifiableList(x).get(i)` over a backing whose layout the
//! ArrayList natives cannot read.
//!
//! `native_unmod_get` bounds-checks before delegating, so that a wrapper over
//! an out-of-range index keeps raising `ArrayIndexOutOfBoundsException` rather
//! than the plain `IndexOutOfBoundsException` the backing `ArrayList` would
//! raise. The size for that check came from `al_state`, whose `(None, 0)`
//! return means **"this is not a layout I can read"** for every backing that is
//! not ArrayList/Vector-shaped — a real-JDK `java/util/LinkedList`, a
//! `cratonvm/internal/ArrayListSubList`, any foreign `AbstractSequentialList` —
//! and is indistinguishable from a genuinely empty ArrayList. Reading it as a
//! size of 0 made `get` throw for **every** index on a list whose `size()`,
//! `iterator()`, `toString()` and `indexOf()` all answered correctly.
//!
//! Fixed by `de3c4d35b` / `unmod_view_size`, which keys the decision on the
//! DATA slot instead: `None` means stand aside and let the delegate answer.
//! That helper has its own unit test in `lib.rs`; these are the end-to-end
//! ones, driving the real registry through `Collections.unmodifiableList`.
//!
//! Live capture: spring-framework
//! `BeanRegistrationsAotContributionTests`, where `SourceFile.getClassName`
//! runs the generated source through QDox and `DefaultJavaSource.getClasses()`
//! hands back `Collections.unmodifiableList(<LinkedList>)`. `size() == 1`
//! passed the assertion on the line above; `get(0)` on the line below threw.
//! See `beanregistrations-verylarge-heap-footprint-FIXED-20260806.md`.

mod common;

use common::{build_registry, call, new_arraylist, MockCtx};
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::Value;

const COLLECTIONS: &str = "java/util/Collections";
const UNMOD_LIST: &str = "cratonvm/internal/UnmodifiableList";
const GET: &str = "(I)Ljava/lang/Object;";
/// Slot 1 of the wrapper — `freeze_result` stamps it for the `List.of` family.
const UNMOD_FIELD_IMMUTABLE: usize = 1;

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
fn a_readable_arraylist_backing_still_bounds_checks() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();

    // An ArrayList backing IS readable, so the pre-check computes a real size
    // (0) — but for a `Collections.unmodifiable*` VIEW it still defers the
    // exception to the delegate, because on HotSpot the backing decides both
    // class and wording. `unmod_list_oob_error` owns that split and is tested
    // next to itself; what is pinned here is that the call reaches the backing
    // rather than being answered from a size the wrapper invented.
    let backing = new_arraylist(&reg, &mut ctx);
    let wrapper = wrap(&reg, &mut ctx, backing);
    ctx.clear_invoke_virtual_log();
    let _ = call(
        &reg,
        &mut ctx,
        UNMOD_LIST,
        "get",
        GET,
        &[Value::Object(Some(wrapper)), Value::Int(0)],
    );
    assert!(
        ctx.invoke_virtual_log()
            .iter()
            .any(|(recv, name, ..)| *recv == backing.as_ptr() as usize && name == "get"),
        "an unmodifiable VIEW must let its backing raise the bounds error",
    );

    // The same wrapper marked immutable is a `List.of`-flavoured receiver, and
    // `ImmutableCollections.ListN` indexes its array directly — so at 0 or 3+
    // elements the subclass is the right answer and the pre-check raises it
    // without a delegate round trip.
    let immutable = wrap(&reg, &mut ctx, backing);
    ctx.set_field(immutable, UNMOD_FIELD_IMMUTABLE, Value::Int(1));
    let err = call(
        &reg,
        &mut ctx,
        UNMOD_LIST,
        "get",
        GET,
        &[Value::Object(Some(immutable)), Value::Int(0)],
    )
    .expect_err("get(0) on an empty List.of-shaped receiver must raise");
    assert!(
        format!("{err:?}").contains("ArrayIndexOutOfBounds"),
        "expected ArrayIndexOutOfBoundsException, got {err:?}",
    );
}

#[test]
fn a_negative_index_over_a_foreign_backing_is_delegated_too() {
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

    // The pre-check stands aside for an unreadable backing at EVERY index, not
    // just the in-range ones: it has no size to compare against, and the
    // delegate's own `get` is the thing that knows. Pinning this because the
    // tempting "well, a negative index is always wrong" special case would
    // reintroduce a second answer for the same question — the delegate's and
    // this one's — which is how the original defect got in.
    ctx.clear_invoke_virtual_log();
    let _ = call(
        &reg,
        &mut ctx,
        UNMOD_LIST,
        "get",
        GET,
        &[Value::Object(Some(wrapper)), Value::Int(-1)],
    );

    let log = ctx.invoke_virtual_log();
    assert!(
        log.iter().any(
            |(recv, name, desc, args)| *recv == backing.as_ptr() as usize
                && name == "get"
                && desc == GET
                && args == &[Value::Int(-1)]
        ),
        "get(-1) must reach the backing rather than being pre-rejected; log = {log:?}",
    );
}

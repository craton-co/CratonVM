// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! The `java/util/AbstractCollection` interception, pinned.
//!
//! `register_arraylist_natives` deliberately registers three natives on the
//! abstract base `java/util/AbstractCollection` — `toArray()`,
//! `toArray(T[])` and `contains(Object)`. Because the interpreter's dispatch
//! walk climbs the receiver's SUPERCLASS chain and asks the native registry at
//! each ancestor *before* it asks whether that ancestor has bytecode
//! (`vm/src/runtime/interpreter/invoke.rs`), those three answer for **every**
//! `Collection` in the VM that does not declare its own override — JDK,
//! third-party and user classes alike.
//!
//! That makes them the widest single interception in the workspace, and it is
//! deliberate: it is what makes `EnumSet.allOf(..).toArray()` and Spring Boot's
//! `Launcher.createClassLoader(c)` (`c.toArray(new URL[0])`) work. There is no
//! "just delete it" escape here — unlike the `AbstractMap.equals`/`hashCode`
//! case, `toArray`/`contains` do NOT exist on `java/lang/Object`, so refusing
//! would surface a `NoSuchMethodError` rather than fall through to a correct
//! implementation.
//!
//! So the requirement is that the implementations answer correctly for a
//! receiver whose layout they have never seen. The documented minimal contract
//! for `AbstractCollection` is "implement `iterator()` and `size()`" — a
//! subclass need expose no readable element storage at all — so these tests
//! drive exactly that receiver.
//!
//! See `docs/feature-designs/collections-interception.md`.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeContext, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{build_registry, call, new_arraylist, MockCtx};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, ObjectRef, Value};

const ABSTRACT_COLLECTION: &str = "java/util/AbstractCollection";
const TO_ARRAY_TYPED: &str = "([Ljava/lang/Object;)[Ljava/lang/Object;";
const TO_ARRAY: &str = "()[Ljava/lang/Object;";

/// A receiver that models the minimal `AbstractCollection` subclass: a class
/// the collection natives have never heard of, with **zero** fields, so every
/// field-layout heuristic in `collect_collection_elements` correctly declines
/// it and the natives are left with nothing but the receiver's own
/// `size()`/`iterator()`.
///
/// The sibling JDK class names are interned first on purpose: the layout
/// guards (`al_is_arraylist_layout`, `is_hashset_native_backed`, …) are
/// *lenient* when a name is unknown to the class manager, so a mock that never
/// mentions `java/util/ArrayList` would let the foreign receiver through the
/// ArrayList probe and stop testing what this file is about.
fn foreign_collection(ctx: &mut MockCtx) -> ObjectRef {
    for name in [
        "java/util/ArrayList",
        "java/util/Vector",
        "java/util/HashSet",
        "java/util/LinkedHashSet",
        "java/util/TreeSet",
        "java/util/LinkedList",
        "java/util/List",
        "java/util/Set",
        "java/util/Collection",
    ] {
        ctx.ensure_class_initialized(name).unwrap();
    }
    let cid = ctx.ensure_class_initialized("test/ForeignBag").unwrap();
    ctx.alloc_object(cid, 0)
}

/// Allocate `n` distinct elements of an application class.
///
/// Field-less on purpose: element comparison (`values_equal`) unboxes wrapper
/// classes and compares enum constants by `(class, ordinal)` before falling
/// back to identity, and a field-less application class cannot be mistaken for
/// either — so these elements compare by identity alone and the tests do not
/// depend on the mock's `equals` dispatch.
fn elements(ctx: &mut MockCtx, n: usize) -> Vec<ObjectRef> {
    let cid = ctx.ensure_class_initialized("test/Element").unwrap();
    (0..n).map(|_| ctx.alloc_object(cid, 0)).collect()
}

/// Script the receiver's own `size()` + `iterator()` walk: this is the only
/// state a minimal `AbstractCollection` subclass exposes.
fn script_size_then_iteration(ctx: &MockCtx, iterator: ObjectRef, elems: &[ObjectRef]) {
    let mut results: Vec<MethodCallResult> = Vec::new();
    results.push(Ok(Some(Value::Int(elems.len() as i32)))); // size()
    results.push(Ok(Some(Value::Object(Some(iterator))))); // iterator()
    for e in elems {
        results.push(Ok(Some(Value::Int(1)))); // hasNext()
        results.push(Ok(Some(Value::Object(Some(*e))))); // next()
    }
    results.push(Ok(Some(Value::Int(0)))); // hasNext() -> done
    ctx.set_invoke_virtual_results(results);
}

/// Script `size()` + `toArray()` — the pair `collect_collection_elements_or_real`
/// drives for an ARGUMENT collection (it deliberately uses `toArray()` rather
/// than `iterator()` to keep itself off the iterator native's path).
fn script_size_then_to_array(ctx: &mut MockCtx, elems: &[ObjectRef]) {
    let arr = ctx.new_ref_array(ClassId::new(0), elems.len());
    for (i, e) in elems.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Object(Some(*e)));
    }
    ctx.set_invoke_virtual_results(vec![
        Ok(Some(Value::Int(elems.len() as i32))),
        Ok(Some(Value::Object(Some(arr)))),
    ]);
}

fn methods_called(ctx: &MockCtx) -> Vec<String> {
    ctx.invoke_virtual_log()
        .into_iter()
        .map(|(_, m, _, _)| m)
        .collect()
}

fn array_contents(ctx: &MockCtx, result: MethodCallResult) -> Vec<Value> {
    let arr = match result.unwrap() {
        Some(Value::Object(Some(a))) => a,
        other => panic!("expected an array, got {other:?}"),
    };
    (0..ctx.array_length(arr))
        .map(|i| ctx.get_array_element(arr, i))
        .collect()
}

/// The bug this file was written for.
///
/// `toArray()` (zero-arg) carried the real-iterator fallback inline; its
/// `toArray(T[])` sibling — registered on the SAME abstract base, intercepting
/// the SAME receivers — called the bare layout reader and so returned a
/// zero-length array for anything it could not decode. That is the failure
/// mode a layout reader has: not a crash, a plausible empty answer.
#[test]
fn to_array_typed_on_an_unmodelled_receiver_uses_its_real_iterator() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let bag = foreign_collection(&mut ctx);
    let elems = elements(&mut ctx, 2);
    let iterator = ctx.alloc_object_simple(0);
    script_size_then_iteration(&ctx, iterator, &elems);

    let result = call(
        &reg,
        &mut ctx,
        ABSTRACT_COLLECTION,
        "toArray",
        TO_ARRAY_TYPED,
        &[Value::Object(Some(bag)), Value::Object(None)],
    );
    let contents = array_contents(&ctx, result);

    assert_eq!(
        contents,
        vec![Value::Object(Some(elems[0])), Value::Object(Some(elems[1])),],
        "AbstractCollection.toArray(T[]) must honour the Collection contract \
         for a receiver whose layout no heuristic models"
    );
}

/// The zero-arg overload already behaved; pin it so collapsing both onto one
/// helper cannot silently regress it.
#[test]
fn to_array_on_an_unmodelled_receiver_uses_its_real_iterator() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let bag = foreign_collection(&mut ctx);
    let elems = elements(&mut ctx, 3);
    let iterator = ctx.alloc_object_simple(0);
    script_size_then_iteration(&ctx, iterator, &elems);

    let result = call(
        &reg,
        &mut ctx,
        ABSTRACT_COLLECTION,
        "toArray",
        TO_ARRAY,
        &[Value::Object(Some(bag))],
    );
    let contents = array_contents(&ctx, result);

    assert_eq!(contents.len(), 3);
    assert_eq!(contents[2], Value::Object(Some(elems[2])));
}

/// `contains` is the third `AbstractCollection` registration, and its wrong
/// answer is the quietest of the three: a flat `false` for an element that IS
/// present, which reads as "the collection does not have it" rather than as a
/// failure.
#[test]
fn contains_on_an_unmodelled_receiver_finds_a_present_element() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let bag = foreign_collection(&mut ctx);
    let elems = elements(&mut ctx, 2);
    let iterator = ctx.alloc_object_simple(0);
    script_size_then_iteration(&ctx, iterator, &elems);

    let found = call(
        &reg,
        &mut ctx,
        ABSTRACT_COLLECTION,
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(bag)), Value::Object(Some(elems[0]))],
    )
    .unwrap();

    assert_eq!(
        found,
        Some(Value::Int(1)),
        "AbstractCollection.contains must consult the receiver's own iterator \
         when no layout heuristic can read its elements"
    );
}

/// The other half of the contract: still `false` for an absent element. Without
/// this, "always true" would pass the test above.
#[test]
fn contains_on_an_unmodelled_receiver_still_rejects_an_absent_element() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let bag = foreign_collection(&mut ctx);
    let elems = elements(&mut ctx, 2);
    let stranger = elements(&mut ctx, 1)[0];
    let iterator = ctx.alloc_object_simple(0);
    script_size_then_iteration(&ctx, iterator, &elems);

    let found = call(
        &reg,
        &mut ctx,
        ABSTRACT_COLLECTION,
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(bag)), Value::Object(Some(stranger))],
    )
    .unwrap();

    assert_eq!(found, Some(Value::Int(0)));
}

/// A genuinely empty foreign collection must not pay for an iterator walk —
/// the `size()` guard is what keeps the fallback off the hot path. Asserting
/// the *absence* of the `iterator()` call is the only way to see that guard;
/// an empty result alone would look identical if the guard were gone.
#[test]
fn an_empty_unmodelled_receiver_is_answered_from_size_alone() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let bag = foreign_collection(&mut ctx);
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(0)))]);

    let result = call(
        &reg,
        &mut ctx,
        ABSTRACT_COLLECTION,
        "toArray",
        TO_ARRAY_TYPED,
        &[Value::Object(Some(bag)), Value::Object(None)],
    );
    let contents = array_contents(&ctx, result);

    assert!(contents.is_empty());
    assert_eq!(
        methods_called(&ctx),
        vec!["size".to_string()],
        "an empty collection must cost exactly one virtual call"
    );
}

/// The fallback must stay a *fallback*: a receiver the layout reader genuinely
/// understands has to be answered from its slots, with no virtual dispatch at
/// all. This is what stops the fix from turning every `toArray` in the VM into
/// an iterator walk.
#[test]
fn a_readable_receiver_is_answered_from_its_layout_without_virtual_calls() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = new_arraylist(&reg, &mut ctx);
    let elems = elements(&mut ctx, 2);
    for e in &elems {
        call(
            &reg,
            &mut ctx,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(*e))],
        )
        .unwrap();
    }
    ctx.clear_invoke_virtual_log();

    let result = call(
        &reg,
        &mut ctx,
        ABSTRACT_COLLECTION,
        "toArray",
        TO_ARRAY_TYPED,
        &[Value::Object(Some(list)), Value::Object(None)],
    );
    let contents = array_contents(&ctx, result);

    assert_eq!(
        contents,
        vec![Value::Object(Some(elems[0])), Value::Object(Some(elems[1])),]
    );
    let called = methods_called(&ctx);
    assert!(
        !called.iter().any(|m| m == "iterator" || m == "size"),
        "a layout-readable receiver must not be re-derived through virtual \
         dispatch, but saw {called:?}"
    );
}

/// `forEach` and `stream` share the same element reader as `toArray`, and
/// their in-situ comments already claimed the iterator fallback. Pin that they
/// really have it now, using `forEach`'s per-element `accept` callbacks as the
/// observable.
#[test]
fn for_each_on_an_unmodelled_receiver_visits_every_element() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let bag = foreign_collection(&mut ctx);
    let elems = elements(&mut ctx, 2);
    let iterator = ctx.alloc_object_simple(0);
    let action = ctx.alloc_object_simple(0);
    script_size_then_iteration(&ctx, iterator, &elems);

    call(
        &reg,
        &mut ctx,
        "java/util/Collection",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        &[Value::Object(Some(bag)), Value::Object(Some(action))],
    )
    .unwrap();

    let visited: Vec<Value> = ctx
        .invoke_virtual_log()
        .into_iter()
        .filter(|(recv, m, _, _)| *recv == action.as_ptr() as usize && m == "accept")
        .map(|(_, _, _, args)| args.first().copied().unwrap_or(Value::Object(None)))
        .collect();

    assert_eq!(
        visited,
        vec![Value::Object(Some(elems[0])), Value::Object(Some(elems[1])),],
        "Collection.forEach must visit the elements of a receiver whose layout \
         no heuristic models"
    );
}

/// The same "empty means both genuinely-empty and undecodable" trap on the
/// ARGUMENT side. `native_al_add_all` read its argument through the
/// real-fallback wrapper; `removeAll` / `retainAll` / `LinkedList.addAll` /
/// `ArrayDeque.addAll` read it through the bare heuristic reader, so a bulk op
/// against an unmodelled collection was a silent no-op that still reported
/// `false`. (`retainAll` was worse than a no-op: an empty argument means
/// "retain nothing".)
#[test]
fn remove_all_reads_an_unmodelled_argument_collection() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = new_arraylist(&reg, &mut ctx);
    let elems = elements(&mut ctx, 2);
    for e in &elems {
        call(
            &reg,
            &mut ctx,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(*e))],
        )
        .unwrap();
    }
    let bag = foreign_collection(&mut ctx);
    script_size_then_to_array(&mut ctx, &elems[..1]);

    let modified = call(
        &reg,
        &mut ctx,
        "java/util/ArrayList",
        "removeAll",
        "(Ljava/util/Collection;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(bag))],
    )
    .unwrap();
    assert_eq!(
        modified,
        Some(Value::Int(1)),
        "removeAll must see the elements of an argument collection whose \
         layout no heuristic models"
    );

    let size = call(
        &reg,
        &mut ctx,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(1)));
}

/// Belt-and-braces on the census itself: these three, and only these three,
/// are what this crate hangs on `java/util/AbstractCollection`. A fourth
/// arriving without an entry in `collections-interception.md` should fail here
/// rather than be discovered by an application.
#[test]
fn abstract_collection_carries_exactly_the_three_audited_natives() {
    let reg: NativeMethodRegistry = build_registry();
    for (method, desc) in [
        ("toArray", TO_ARRAY),
        ("toArray", TO_ARRAY_TYPED),
        ("contains", "(Ljava/lang/Object;)Z"),
    ] {
        let base = ABSTRACT_COLLECTION;
        assert!(
            reg.find(base, method, desc).is_some(),
            "{base}.{method}{desc} is load-bearing (EnumSet.toArray, \
             Spring Boot Launcher's toArray(new URL[0])) — do not drop it without \
             updating docs/feature-designs/collections-interception.md"
        );
    }
    // `java/util/AbstractSet` carries exactly one: `hashCode`. Its sibling
    // `equals` must NOT be intercepted — the real `AbstractSet.equals`
    // bytecode is correct and there is no Object-level fallback that is.
    assert!(
        reg.find("java/util/AbstractSet", "hashCode", "()I")
            .is_some(),
        "AbstractSet.hashCode is the element-hash-sum implementation of the \
         Set.hashCode contract; see collections-interception.md"
    );
    assert!(
        reg.find("java/util/AbstractSet", "equals", "(Ljava/lang/Object;)Z")
            .is_none(),
        "AbstractSet.equals must stay unregistered so the real bytecode runs"
    );
    // The remaining abstract `java.util` bases must stay empty: a user
    // `class X extends AbstractList` has to inherit real bytecode only.
    for base in [
        "java/util/AbstractList",
        "java/util/AbstractSequentialList",
        "java/util/AbstractQueue",
        "java/util/AbstractMap",
    ] {
        for (method, desc) in [
            ("hashCode", "()I"),
            ("equals", "(Ljava/lang/Object;)Z"),
            ("toString", "()Ljava/lang/String;"),
            ("toArray", TO_ARRAY),
            ("contains", "(Ljava/lang/Object;)Z"),
        ] {
            assert!(
                reg.find(base, method, desc).is_none(),
                "{base}.{method}{desc} is a new inheritance-wide interception — \
                 add it to docs/feature-designs/collections-interception.md first"
            );
        }
    }
}

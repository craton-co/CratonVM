// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Behavioural coverage for `java.util.ArrayList` natives.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{boxed_int, build_registry, call, new_arraylist, MockCtx};
use cratonvm_native_api::NativeContext;
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::{ClassId, Value};

const AL: &str = "java/util/ArrayList";

#[test]
fn arrays_sort_accepts_comparable_lambda_proxy() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let comparable = ctx
        .ensure_class_initialized("java/lang/Comparable")
        .unwrap();
    let functional_interface = ctx
        .ensure_class_initialized("test/FunctionalComparable")
        .unwrap();
    ctx.set_class_interfaces(functional_interface, vec![comparable]);

    // A real lambda proxy is not a class-manager class. Its interface must be
    // resolved through lambda metadata rather than `class_interfaces(proxy)`.
    let lambda_class = ClassId::new(0x8000_0001);
    ctx.set_lambda_proxy_metadata(
        lambda_class,
        "test/FunctionalComparable",
        "test/LambdaSortProbe",
    );
    let lambda = ctx.alloc_object_simple(lambda_class.as_u32());
    let array = ctx.new_ref_array(ClassId::new(0), 1);
    ctx.set_array_element(array, 0, Value::Object(Some(lambda)));

    let result = call(
        &reg,
        &mut ctx,
        "java/util/Arrays",
        "sort",
        "([Ljava/lang/Object;)V",
        &[Value::Object(Some(array))],
    );
    assert!(
        result.is_ok(),
        "Comparable lambda proxy must pass Arrays.sort precheck: {result:?}"
    );
}

#[test]
fn arrays_sort_rejects_non_comparable_lambda_proxy() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    ctx.ensure_class_initialized("test/PlainFunction").unwrap();
    let lambda_class = ClassId::new(0x8000_0002);
    ctx.set_lambda_proxy_metadata(lambda_class, "test/PlainFunction", "test/LambdaSortProbe");
    let lambda = ctx.alloc_object_simple(lambda_class.as_u32());
    let array = ctx.new_ref_array(ClassId::new(0), 1);
    ctx.set_array_element(array, 0, Value::Object(Some(lambda)));

    let error = call(
        &reg,
        &mut ctx,
        "java/util/Arrays",
        "sort",
        "([Ljava/lang/Object;)V",
        &[Value::Object(Some(array))],
    )
    .expect_err("non-Comparable lambda proxy must fail Arrays.sort");
    let MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::ClassCastException {
        message,
    })) = error
    else {
        panic!("expected ClassCastException, got {error:?}");
    };
    assert!(message.contains("test/LambdaSortProbe$$Lambda/0x80000002"));
    assert!(!message.contains("<unknown>"));
}

#[test]
fn empty_size_is_zero_and_is_empty_true() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let al = new_arraylist(&reg, &mut ctx);

    let size = call(
        &reg,
        &mut ctx,
        AL,
        "size",
        "()I",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(0)));

    let empty = call(
        &reg,
        &mut ctx,
        AL,
        "isEmpty",
        "()Z",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    assert_eq!(empty, Some(Value::Int(1)));
}

#[test]
fn single_add_get() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let al = new_arraylist(&reg, &mut ctx);

    let v = boxed_int(&mut ctx, 42);
    let added = call(
        &reg,
        &mut ctx,
        AL,
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), v],
    )
    .unwrap();
    // add returns boolean true (1 in Value::Int).
    assert_eq!(added, Some(Value::Int(1)));

    let got = call(
        &reg,
        &mut ctx,
        AL,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(al)), Value::Int(0)],
    )
    .unwrap();
    assert_eq!(got, Some(v));

    let size = call(
        &reg,
        &mut ctx,
        AL,
        "size",
        "()I",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(1)));
}

#[test]
fn n_element_add_get_size_remove_iterator() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let al = new_arraylist(&reg, &mut ctx);

    // Add 20 elements.
    let mut vals = Vec::with_capacity(20);
    for i in 0..20 {
        let v = boxed_int(&mut ctx, i + 1);
        vals.push(v);
        call(
            &reg,
            &mut ctx,
            AL,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(al)), v],
        )
        .unwrap();
    }

    let size = call(
        &reg,
        &mut ctx,
        AL,
        "size",
        "()I",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(20)));

    // get each element.
    for (i, v) in vals.iter().enumerate() {
        let got = call(
            &reg,
            &mut ctx,
            AL,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(al)), Value::Int(i as i32)],
        )
        .unwrap();
        assert_eq!(got, Some(*v), "get({i}) mismatch");
    }

    // remove the head — every subsequent element shifts down by one.
    let removed = call(
        &reg,
        &mut ctx,
        AL,
        "remove",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(al)), Value::Int(0)],
    )
    .unwrap();
    assert_eq!(removed, Some(vals[0]));

    let size_after = call(
        &reg,
        &mut ctx,
        AL,
        "size",
        "()I",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    assert_eq!(size_after, Some(Value::Int(19)));

    let got0 = call(
        &reg,
        &mut ctx,
        AL,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(al)), Value::Int(0)],
    )
    .unwrap();
    assert_eq!(
        got0,
        Some(vals[1]),
        "after remove(0), get(0) should be old vals[1]"
    );

    // iterator: walk the remaining 19 elements.
    let iter_v = call(
        &reg,
        &mut ctx,
        AL,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    let iter = match iter_v {
        Some(Value::Object(Some(o))) => o,
        other => panic!("iterator() returned {:?}", other),
    };

    let mut count = 0usize;
    loop {
        let hn = call(
            &reg,
            &mut ctx,
            "java/util/ArrayList$Itr",
            "hasNext",
            "()Z",
            &[Value::Object(Some(iter))],
        )
        .unwrap();
        match hn {
            Some(Value::Int(n)) if n != 0 => {}
            _ => break,
        }
        let n = call(
            &reg,
            &mut ctx,
            "java/util/ArrayList$Itr",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(iter))],
        )
        .unwrap();
        match n {
            Some(Value::Object(Some(_))) => count += 1,
            _ => break,
        }
        if count > 1000 {
            panic!("iterator runaway");
        }
    }
    assert_eq!(
        count, 19,
        "iterator should visit 19 elements after remove(0)"
    );
}

/// `java.util.ArrayList.get` out of range throws the PLAIN
/// `IndexOutOfBoundsException`, not the `ArrayIndexOutOfBoundsException`
/// subclass — measured on Temurin 25 with `probes/ListOutOfBoundsProbe`:
///
/// ```text
/// new ArrayList().get(0)    IndexOutOfBoundsException: Index 0 out of bounds for length 0
/// arrayListOf2.get(5)       IndexOutOfBoundsException: Index 5 out of bounds for length 2
/// arrayListOf2.get(-1)      IndexOutOfBoundsException: Index -1 out of bounds for length 2
/// ```
///
/// The subclass is the direction that breaks a `catch`. This test asserted the
/// subclass and went red when `native_al_get` was corrected on 2026-08-05; it
/// was the test that was stale, not the implementation. `native_unmod_get`'s
/// AIOOBE on the *unmodifiable wrapper* path is a different receiver with a
/// different measured answer — see the note there.
#[test]
fn get_out_of_bounds_throws_ioobe() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let al = new_arraylist(&reg, &mut ctx);

    // Empty list: any index throws the plain IndexOutOfBoundsException.
    let result = call(
        &reg,
        &mut ctx,
        AL,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(al)), Value::Int(0)],
    );
    let err = result.expect_err("get on empty list must throw");
    assert!(
        is_ioobe(&err, "Index 0 out of bounds for length 0"),
        "expected IndexOutOfBoundsException with HotSpot's wording, got {err:?}"
    );

    // Non-empty, but request past end.
    let v = boxed_int(&mut ctx, 7);
    call(
        &reg,
        &mut ctx,
        AL,
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), v],
    )
    .unwrap();
    let result = call(
        &reg,
        &mut ctx,
        AL,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(al)), Value::Int(5)],
    );
    let err = result.expect_err("get past end must throw");
    assert!(
        is_ioobe(&err, "Index 5 out of bounds for length 1"),
        "expected IndexOutOfBoundsException(5) on a size-1 list, got {err:?}"
    );

    // Negative index: the same plain class and wording.
    let result = call(
        &reg,
        &mut ctx,
        AL,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(al)), Value::Int(-1)],
    );
    let err = result.expect_err("get(-1) must throw");
    assert!(
        is_ioobe(&err, "Index -1 out of bounds for length 1"),
        "expected IndexOutOfBoundsException(-1) on a size-1 list, got {err:?}"
    );
}

// Backed-view class returned by `ArrayList.subList(int,int)` (fix item 6: a
// view sharing the parent's backing array, NOT a detached copy). It carries its
// own size/get/set natives; `size()`/`get()` on the returned object dispatch on
// THIS class, not on ArrayList.
const ASL: &str = "cratonvm/internal/ArrayListSubList";

#[test]
fn sublist_returns_a_smaller_list() {
    // subList(fromIndex, toIndex) returns a backed view over the parent slice.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let al = new_arraylist(&reg, &mut ctx);

    let mut vals = Vec::new();
    for i in 0..10 {
        let v = boxed_int(&mut ctx, i);
        vals.push(v);
        call(
            &reg,
            &mut ctx,
            AL,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(al)), v],
        )
        .unwrap();
    }

    let sub = call(
        &reg,
        &mut ctx,
        AL,
        "subList",
        "(II)Ljava/util/List;",
        &[Value::Object(Some(al)), Value::Int(2), Value::Int(7)],
    )
    .unwrap();
    let sub_obj = match sub {
        Some(Value::Object(Some(o))) => o,
        other => panic!("subList returned {:?}", other),
    };
    // The view is an ArrayListSubList instance — query it via its own natives.
    let sub_size = call(
        &reg,
        &mut ctx,
        ASL,
        "size",
        "()I",
        &[Value::Object(Some(sub_obj))],
    )
    .unwrap();
    assert_eq!(
        sub_size,
        Some(Value::Int(5)),
        "subList(2, 7) covers 5 elements"
    );

    let sub0 = call(
        &reg,
        &mut ctx,
        ASL,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(sub_obj)), Value::Int(0)],
    )
    .unwrap();
    assert_eq!(
        sub0,
        Some(vals[2]),
        "subList(2,7).get(0) is the original element at index 2"
    );
}

#[test]
fn iterator_remove_drops_element() {
    // ArrayList iterator: hasNext, next, remove. After remove, the
    // backing list should shrink by one.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let al = new_arraylist(&reg, &mut ctx);
    let v0 = boxed_int(&mut ctx, 10);
    let v1 = boxed_int(&mut ctx, 20);
    let v2 = boxed_int(&mut ctx, 30);
    for v in [v0, v1, v2] {
        call(
            &reg,
            &mut ctx,
            AL,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(al)), v],
        )
        .unwrap();
    }
    let iter_v = call(
        &reg,
        &mut ctx,
        AL,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    let iter = match iter_v {
        Some(Value::Object(Some(o))) => o,
        other => panic!("iterator returned {:?}", other),
    };

    // next → get the first element, then call remove.
    let _ = call(
        &reg,
        &mut ctx,
        "java/util/ArrayList$Itr",
        "next",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(iter))],
    )
    .unwrap();
    let _ = call(
        &reg,
        &mut ctx,
        "java/util/ArrayList$Itr",
        "remove",
        "()V",
        &[Value::Object(Some(iter))],
    );

    // Lock in the invariant the dispatcher guarantees: after the iterator's
    // own `remove`, the *backing list* size dropped to 2.
    let size_after = call(
        &reg,
        &mut ctx,
        AL,
        "size",
        "()I",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    assert_eq!(
        size_after,
        Some(Value::Int(2)),
        "iterator.remove() must shrink the backing list by 1"
    );
}

/// Pattern-match an `ArrayIndexOutOfBoundsException(idx)` inside the
/// `Err(MethodCallFailed::InternalError(VmError::Runtime(...)))` shell
/// the natives surface.
fn is_aioobe(err: &MethodCallFailed, expected: i32) -> bool {
    matches!(err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ArrayIndexOutOfBoundsException { index, .. })) if *index == expected)
}

/// The same shell, for the plain `IndexOutOfBoundsException` superclass — which
/// is what `java.util.ArrayList` throws on HotSpot. Checks the message too: the
/// class and the wording are two different halves of the contract, and this
/// family has already shipped a right-class/wrong-message state once.
fn is_ioobe(err: &MethodCallFailed, expected_message: &str) -> bool {
    matches!(err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IndexOutOfBoundsException { message: Some(m) }))
            if m == expected_message)
}

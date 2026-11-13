// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT tier2 wave 5, lane nat21 (`NATIVE21-2`): `Thread.holdsLock(Object)` is
//! claimed LEAF (`NativeMethodRegistry::set_leaf`). The claim rides on the
//! callback, so both registrations of the triple must carry it; the leaf body
//! must stay leaf-safe (no allocation, no blocking, no Java re-entry) and must
//! decline BEFORE running the full body.

use cratonvm_native_api::test_mock::MockNativeContext;
use cratonvm_native_api::{NativeHeapAccess, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::Value;

const CLASS: &str = "java/lang/Thread";
const METHOD: &str = "holdsLock";
const DESC: &str = "(Ljava/lang/Object;)Z";

fn body<'a>(src: &'a str, head: &str) -> &'a str {
    let start = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("end of fn");
    &rest[..end]
}

#[test]
fn nat21_holds_lock_is_registered_leaf_and_stays_a_bridge() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.leaf_registrations().contains(&(CLASS, METHOD, DESC)),
        "Thread.holdsLock lost its leaf claim (the last registration of the triple decides)"
    );
}

#[test]
fn nat21_the_leaf_body_stays_leaf_safe_and_declines_before_the_full_body() {
    let src = include_str!("../src/lib.rs");
    let leaf = body(src, "fn native_thread_holds_lock_leaf(");
    let forbidden = [
        "new_object",
        "alloc_object",
        "new_array",
        "new_ref_array",
        "create_string",
        "intern_string",
        "begin_blocking_region",
        "invoke(",
        "invoke_virtual",
        "invoke_static",
        "pin_native_root",
        "ensure_class_initialized",
        "initialize_class",
        "load_class",
        "maybe_gc",
        "monitor_enter",
        "current_thread_holds_lock(",
    ];
    for line in leaf.lines() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        for f in forbidden {
            assert!(
                !t.contains(f),
                "`{f}` in native_thread_holds_lock_leaf: {t}"
            );
        }
    }
    let quick = leaf
        .find("current_thread_holds_lock_lock_free(")
        .expect("the leaf reads the lock-free answer");
    let decline = leaf.find("decline_leaf_call()").expect("it declines");
    let full = leaf
        .rfind("native_thread_holds_lock(ctx, args)")
        .expect("it falls back to the funnel body");
    assert!(
        quick < decline && decline < full,
        "order: lock-free read, decline, full body"
    );
}

/// With the default `NativeContext` (no lock-free answer, no leaf path) the
/// leaf callback runs the full body: `false` for an object the mock holds no
/// monitor of, HotSpot's message-less NPE for `null`, `false` for a non-object.
#[test]
fn nat21_the_callback_falls_back_to_the_full_answer() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let f = r.find(CLASS, METHOD, DESC).expect("registered");
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(cratonvm_types::ClassId::new(0), 0);
    let answer = |ctx: &mut MockNativeContext, arg: Value| f(ctx, &[arg]).ok();
    assert_eq!(
        answer(&mut ctx, Value::Object(Some(obj))),
        Some(Some(Value::Int(0)))
    );
    match f(&mut ctx, &[Value::Object(None)]) {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message },
        ))) => assert_eq!(message, None, "`JVM_HoldsLock` throws with no message"),
        other => panic!("expected the NPE, got {other:?}"),
    }
    assert_eq!(answer(&mut ctx, Value::Int(3)), Some(Some(Value::Int(0))));
}
